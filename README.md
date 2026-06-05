# ai-api-bridge

A local format-translating AI API proxy. It presents the OpenAI **Responses API**
to clients (e.g. the Codex CLI) and translates each call into an OpenAI
**Chat Completions** request to an upstream provider (e.g. your OpenCode Zen
account), then translates the streaming response back.

## Quickstart

```bash
cp bridge.example.toml bridge.toml
export BRIDGE_PROVIDERS_ZEN_API_KEY="<your opencode-go Zen key>"
cargo run --release -- --config bridge.toml
# listening on 127.0.0.1:8282
```

Point Codex at it (`~/.codex/config.toml`):

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

## Endpoints
- `POST /v1/responses` — OpenAI Responses API (for Codex).
- `POST /v1/chat/completions` — OpenAI Chat Completions (passthrough).
- `GET /v1/models` — configured aliases.
- `POST /v1/messages` — Anthropic Messages (not implemented in v1; returns 501).
- `GET /health`.

## Config
See `bridge.example.toml`. The Zen key may be set inline (`api_key` under a provider)
or via `BRIDGE_PROVIDERS_<NAME>_API_KEY` (e.g. `BRIDGE_PROVIDERS_ZEN_API_KEY`).
Model names map to the upstream by the provider's `model_prefix`, or per-alias via
`[[routes]]`.

## Architecture
See `docs/superpowers/specs/2026-06-05-ai-api-bridge-design.md` and
`docs/superpowers/plans/2026-06-05-ai-api-bridge.md`.
