# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is
A local format-translating AI API proxy. Clients speak the OpenAI **Responses API**
(`POST /v1/responses`); the bridge translates to OpenAI **Chat Completions**
(`/chat/completions`) against a configured upstream (OpenCode Zen) and translates the
streaming response back. Purpose: use an OpenCode Zen account inside the Codex CLI
(Codex only speaks `wire_api = "responses"`).

## Commands
- Build: `cargo build` (release: `cargo build --release`)
- Run: `cargo run -- --config bridge.toml` (override port: `--listen 127.0.0.1:8282`)
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
- `wire/chat.rs` — Chat **outbound**: `build_request`, `ChatStreamParser` (CC stream chunks
  -> canonical events, including surfacing an in-band `{"error":...}` as
  `CanonicalEvent::Error`), `completion_to_events` (non-stream CC response -> canonical
  events). Also `parse_request` for CC **inbound**.
- `sse.rs` — `SseDecoder` works on **bytes** and only decodes UTF-8 on complete `\n\n`
  event blocks, so a multibyte character split across network chunks is never corrupted.
- `server.rs` — `stream_sse` is the streaming pipeline: SseDecoder -> ChatStreamParser ->
  ResponsesEmitter; one upstream chunk can yield several Responses SSE frames. An in-band
  error or `[DONE]` terminates the stream. The chat-inbound endpoint forwards upstream
  bytes verbatim via `Body::from_stream` (no re-encoding).
- `router.rs` — explicit `[[routes]]` win; otherwise the default provider's `model_prefix`
  is applied (`gpt-5.5` -> `opencode/gpt-5.5`) unless the alias already contains `/`.
- `upstream.rs` — reqwest client; `post_stream` (SSE) and `post_json` (non-stream).

## Endpoints
`POST /v1/responses` (primary, for Codex) · `POST /v1/chat/completions` (passthrough) ·
`GET /v1/models` · `POST /v1/messages` (Anthropic — 501 stub) · `GET /health`.

## Conventions / gotchas
- Wire-format dispatch is enum + plain functions, not async trait objects — keeps the
  streaming code simple. Adding a format = fill its parse/emit/build/parse functions and a
  route; the canonical layer is unchanged.
- `ResponsesEmitter` item IDs are random UUIDs, so tests assert on event **names** and
  payload fields, not IDs.
- `post_stream` returns `impl Stream<...> + Send + 'static` — the explicit bound is required
  (axum spawns the SSE body on the multithreaded runtime).
- Known limitation: no cross-turn encrypted-reasoning reuse (Chat Completions is stateless);
  inbound `reasoning` items are dropped. See spec §11.
- Client auth: if `auth_token` is set in config, clients must send
  `Authorization: Bearer <token>`; otherwise any/no token is accepted.

## Spec & plan
- Spec: `docs/superpowers/specs/2026-06-05-ai-api-bridge-design.md`
- Plan: `docs/superpowers/plans/2026-06-05-ai-api-bridge.md`
## CI / Release
- CI (`.github/workflows/ci.yml`): test + fmt + clippy + build on every push/PR to `main`.
- Release (`.github/workflows/release.yml`): push a `v*` tag → cross builds x86_64 + aarch64 musl static binaries, packages as `.tar.gz`, creates a GitHub Release with changelog.
- Cross-compile locally: `cross build --release --target x86_64-unknown-linux-musl` (x86_64) or `aarch64-unknown-linux-musl` (ARM64). See `Cross.toml`.
