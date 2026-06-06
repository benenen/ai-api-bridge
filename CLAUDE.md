# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is
A local format-translating AI API proxy that serves **two clients at once** from one
upstream account:
- **Codex** → `POST /v1/responses` (OpenAI Responses API)
- **Claude Code** → `POST /v1/messages` (Anthropic Messages API, via `ANTHROPIC_BASE_URL`)

Both are translated to OpenAI **Chat Completions** (`/chat/completions`) against a configured
upstream (OpenCode Zen / the `go` package) and the streaming response is translated back. The
two clients share the same `[[routes]]`, so one alias (e.g. `gpt-5.5 → go/deepseek-v4-pro`)
backs both. Purpose: use an OpenCode Zen account inside both Codex and Claude Code.

## Commands
- Build: `cargo build` (release: `cargo build --release`)
- Run: `cargo run -- --config bridge.toml` (override port: `--listen 127.0.0.1:8282`,
  DB path: `--db bridge.db`)
- Re-import providers/routes from the config into the DB: `cargo run -- --reseed`
- Test (all): `cargo test`
- One test: `cargo test wire::responses::tests::emits_message_sequence_for_text`
- One module: `cargo test wire::chat::tests`
- Verbose logs: `RUST_LOG=ai_api_bridge=debug cargo run -- --config bridge.toml`
- Cross-compile (Linux musl): `cross build --release --target x86_64-unknown-linux-musl`
- Cross-compile (ARM64): `cross build --release --target aarch64-unknown-linux-musl`

## Architecture (needs several files to see the whole)
Translation always goes through a provider-neutral middle layer — never wire-format to
wire-format directly:

  inbound format -> `CanonicalRequest` -> `router::resolve` -> outbound format
  -> upstream HTTP -> upstream byte stream -> `SseDecoder` -> `CanonicalEvent` stream
  -> inbound emitter -> client

- `canonical.rs` — the contract (`CanonicalRequest`, `CanonicalEvent`). Changing it ripples
  to every wire module.
- `wire/responses.rs` — Responses **inbound**: `parse_request` (client req -> canonical) and
  `ResponsesEmitter` (canonical events -> Responses SSE frames; `final_response()` for the
  non-streaming object).
- `wire/anthropic.rs` — Anthropic Messages **inbound** (for Claude Code): `parse_request`
  and `AnthropicEmitter` (canonical events -> `message_start`/`content_block_*`/
  `message_delta`/`message_stop`; `final_message()` for non-streaming). DeepSeek
  `reasoning_content` <-> Anthropic `thinking` blocks.
- `wire/mod.rs` — shared `SseFrame` + the `CanonicalEmitter` trait both emitters implement.
- `wire/chat.rs` — Chat **outbound**: `build_request`, `ChatStreamParser` (CC stream chunks
  -> canonical events, including surfacing an in-band `{"error":...}` as
  `CanonicalEvent::Error`), `completion_to_events` (non-stream CC response -> canonical
  events). Also `parse_request` for CC **inbound**.
- `sse.rs` — `SseDecoder` works on **bytes** and only decodes UTF-8 on complete `\n\n`
  event blocks, so a multibyte character split across network chunks is never corrupted.
- `server.rs` — `run_stream` is the streaming pipeline, generic over the
  `wire::CanonicalEmitter` trait (impl'd by `ResponsesEmitter` and `AnthropicEmitter`):
  SseDecoder -> ChatStreamParser -> emitter; one upstream chunk can yield several client SSE
  frames. An in-band error or `[DONE]` terminates the stream. The chat-inbound endpoint
  forwards upstream bytes verbatim via `Body::from_stream` (no re-encoding).
- `router.rs` — explicit `[[routes]]` win; otherwise the default provider's `model_prefix`
  is applied (`gpt-5.5` -> `opencode/gpt-5.5`) unless the alias already contains `/`.
  `resolve_candidates` returns the **ordered candidate chain** (route primary + `fallback`,
  watcher-usable first) for failover. The server's `open_upstream_stream`/`call_upstream_json`
  try them in order and **reactively** advance on a call-time failure (`is_retryable`:
  connect/timeout, 5xx, 429, 401/402 — not other 4xx); for streaming the retry happens before
  the first byte (headers known first). A reactive failure marks the provider down (+ one-shot
  re-probe via `mark_degraded`). `upstream::ByteStream` is the boxed stream the helpers return.
- `upstream.rs` — reqwest client; `post_stream` (SSE) and `post_json` (non-stream).
- `store.rs` — SQLite (sqlx, runtime queries) for providers + routes + `provider_status`.
  `bridge.toml` seeds an empty DB once (`seed_from_config`), then the DB is authoritative;
  `main` runs `open` → seed-if-`is_empty` → `load_into_config` → `apply_env_overrides` at
  startup (no per-request DB hit). The pool **stays alive** for the watcher's writes.
  `migrations/` holds the schema; `--reseed` clears + re-imports.
- `probe.rs` — `run_probe(name, provider)`: runs the provider's Lua `probe_script` in
  `spawn_blocking` (injects `ctx` + `http{}` via `reqwest::blocking` + `json_decode/encode`;
  returns `{ok,remaining,used,limit,note}`), or a connectivity ping if there's no script.
- `watcher.rs` — one background task per probe-enabled provider on its interval; writes each
  result to the DB + the shared `StatusMap` (`Arc<RwLock<HashMap<String, ProviderStatus>>>`,
  also held in `AppState`), logging availability changes. The router and `/v1/providers`
  read this map.

## Endpoints
`POST /v1/responses` (Codex) · `POST /v1/messages` (Anthropic Messages, for Claude Code) ·
`POST /v1/chat/completions` (passthrough) · `GET /v1/models` ·
`GET /v1/providers` (watcher status) · `GET /health`.

## Conventions / gotchas
- Wire-format dispatch is plain functions + a sync `CanonicalEmitter` trait (used as a
  generic bound, not `dyn`/async) — keeps the streaming code simple. Adding a format = fill
  its parse/emit/build/parse functions and a route; the canonical layer is unchanged.
- `ResponsesEmitter` item IDs are random UUIDs, so tests assert on event **names** and
  payload fields, not IDs.
- `post_stream` returns `impl Stream<...> + Send + 'static` — the explicit bound is required
  (axum spawns the SSE body on the multithreaded runtime).
- Reasoning round-trip: inbound `reasoning` (Responses) / `thinking` (Anthropic) items are
  attached to the assistant turn they belong to and echoed back as `reasoning_content` —
  DeepSeek thinking models require it on every assistant message across multi-step tool turns
  (see `parse_input_item` in responses.rs and the assistant branch in anthropic.rs).
- Client auth: if `auth_token` is set, clients send it as `Authorization: Bearer <token>` or
  `x-api-key: <token>` (Anthropic clients); otherwise any/no token is accepted.

## Spec & plan
- Spec: `docs/superpowers/specs/2026-06-05-ai-api-bridge-design.md`
- Plan: `docs/superpowers/plans/2026-06-05-ai-api-bridge.md`

## CI / Release
- CI (`.github/workflows/ci.yml`): test + fmt + clippy + build on every push/PR to `main`.
- Release (`.github/workflows/release.yml`): push a `v*` tag → builds Linux (x86_64 + aarch64 musl static, via `cross`), Windows (x86_64-msvc) and macOS (arm64 + x86_64) binaries, packages as `.tar.gz` (`.zip` on Windows), creates a GitHub Release with changelog.
- Cross-compile locally: `cross build --release --target x86_64-unknown-linux-musl` (x86_64) or `aarch64-unknown-linux-musl` (ARM64). See `Cross.toml`.

## Docker
- Build locally: `docker build -t ai-api-bridge .`
- Run: `docker run -d -p 8282:8282 -v ./bridge.toml:/etc/ai-api-bridge/bridge.toml ai-api-bridge`
- Release workflow also pushes multi-arch (amd64 + arm64) images to `ghcr.io/<org>/ai-api-bridge`.
