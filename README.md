# ai-api-bridge

A local format-translating AI API proxy. It presents the OpenAI **Responses API**
(for the Codex CLI) and the Anthropic **Messages API** (for Claude Code) to clients,
translates each call into an OpenAI **Chat Completions** request to an upstream provider
(e.g. your OpenCode Zen account), then translates the streaming response back.

## Quickstart

```bash
cp bridge.example.toml bridge.toml
export BRIDGE_PROVIDERS_ZEN_API_KEY="<your opencode-go Zen key>"
cargo run --release -- --config bridge.toml
# listening on 127.0.0.1:8282
```

**One running bridge serves both Codex and Claude Code at the same time.** They hit different
endpoints but share the same `[[routes]]`, so a single alias (`gpt-5.5 → go/deepseek-v4-pro`)
backs both:

| Client | Endpoint | Connect via | Model | Auth header |
|---|---|---|---|---|
| **Codex** | `/v1/responses` | `~/.codex/config.toml` (`wire_api = "responses"`) | `model = "gpt-5.5"` | `Authorization: Bearer` |
| **Claude Code** | `/v1/messages` | `ANTHROPIC_BASE_URL` env | `ANTHROPIC_MODEL=gpt-5.5` | `x-api-key` |

### Codex (`~/.codex/config.toml`)

```toml
model_provider = "bridge"
model = "gpt-5.5"

[model_providers.bridge]
name = "bridge"
base_url = "http://127.0.0.1:8282/v1"
wire_api = "responses"
env_key = "BRIDGE_KEY"   # value ignored unless `auth_token` is set in bridge.toml
```

```bash
BRIDGE_KEY=x codex
```

### Claude Code (Anthropic Messages API via `ANTHROPIC_BASE_URL`)

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8282
export ANTHROPIC_API_KEY=<bridge auth_token, or any value if auth_token is unset>
export ANTHROPIC_MODEL=gpt-5.5    # must match a [[routes]] alias
claude
```

## Endpoints
- `POST /v1/responses` — OpenAI Responses API (for Codex).
- `POST /v1/messages` — Anthropic Messages API (for Claude Code).
- `POST /v1/chat/completions` — OpenAI Chat Completions (passthrough).
- `GET /v1/models` — configured aliases.
- `GET /v1/providers` — per-provider availability + quota (the watcher).
- `GET /health`.

## Monitoring & failover
A background watcher probes each provider's availability and quota (via a per-provider Lua
script — see [`probes/`](probes) and `docs/configuration.md`) and exposes it at
`GET /v1/providers`. A route can declare a `fallback` chain; the bridge fails over both
**proactively** (skipping providers the watcher marks down / below `quota_min`) and
**reactively** (if a provider errors mid-request — connection failure, 5xx, 429, 401/402 — it
switches to the next candidate before the first byte reaches the client).

## Config
See [`bridge.example.toml`](bridge.example.toml) for a working template and
**[`docs/configuration.md`](docs/configuration.md) for the full reference** (every key,
the `go` package model list, routing rules, and Codex + Claude Code setup).

In short: keys are set inline (`api_key` under a provider) or via
`BRIDGE_PROVIDERS_<NAME>_API_KEY` (e.g. `BRIDGE_PROVIDERS_GO_API_KEY`); model names map to
the upstream by the provider's `model_prefix`, or per-alias via `[[routes]]`.

**Providers + routes are stored in SQLite** (`bridge.db`): `bridge.toml` seeds the DB on
first run, then the DB is authoritative (re-run with `--reseed` to re-import from the file).
`listen` / `default_provider` / `auth_token` stay in `bridge.toml`.

## Architecture
See `docs/superpowers/specs/2026-06-05-ai-api-bridge-design.md` and
`docs/superpowers/plans/2026-06-05-ai-api-bridge.md`.
