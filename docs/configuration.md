# Configuration

`ai-api-bridge` is configured by a single TOML file. A working starting point is
[`bridge.example.toml`](../bridge.example.toml) — copy it to `bridge.toml` and edit.

```bash
cp bridge.example.toml bridge.toml
export BRIDGE_PROVIDERS_GO_API_KEY=...        # your go-package key
cargo run --release -- --config bridge.toml   # listens on 127.0.0.1:8282
```

## Clients — Codex and Claude Code at the same time

One running bridge serves **both** clients simultaneously. They use different inbound
endpoints (and different auth headers) but the **same `[[routes]]`**, so a single alias such
as `gpt-5.5 → go/deepseek-v4-pro` backs both:

| Client | Endpoint | Connect via | Model | Auth header |
|---|---|---|---|---|
| **Codex** | `POST /v1/responses` | `~/.codex/config.toml` (`wire_api = "responses"`) | `model` | `Authorization: Bearer` |
| **Claude Code** | `POST /v1/messages` | `ANTHROPIC_BASE_URL` env var | `ANTHROPIC_MODEL` | `x-api-key` |

Setup for each: [Pointing Codex at the bridge](#pointing-codex-at-the-bridge) ·
[Pointing Claude Code at the bridge](#pointing-claude-code-at-the-bridge).

## Loading

- The file is selected with `--config <path>` (default: `bridge.toml` in the working dir).
- `--listen <host:port>` overrides the `listen` value from the file.
- Secrets can be supplied by environment variable instead of being written into the
  file (see [Provider keys](#provider-keys)). **Keep `bridge.toml` out of git** — it is
  already in `.gitignore` because it holds API keys.

## Top-level keys

| Key | Type | Default | Meaning |
|---|---|---|---|
| `listen` | string | `127.0.0.1:8282` | Address the bridge binds to. |
| `default_provider` | string | — | Provider used when no `[[routes]]` entry matches. Optional, but without it any unrouted model name is a 400. |
| `auth_token` | string | — | If set, clients must send `Authorization: Bearer <auth_token>`. If unset, any/no token is accepted. |
| `[providers.<name>]` | table | — | One or more upstream providers (see below). |
| `[[routes]]` | array | — | Model alias → provider/model mappings (see below). |

```toml
listen = "127.0.0.1:8282"
default_provider = "zen"
auth_token = "a-long-random-string"   # optional
```

## Providers

Each `[providers.<name>]` block defines one upstream target. `<name>` is referenced by
routes and by the key env var.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `wire` | enum | — | Upstream wire format. Use `openai-chat` (the implemented outbound format). `openai-responses` and `anthropic-messages` are reserved for future use. |
| `base_url` | string | — | Upstream base URL. The bridge appends `/chat/completions`. |
| `api_key` | string | — | Bearer key for the upstream. Prefer the env var below. |
| `model_prefix` | string | `""` | Prepended to the model name when a route doesn't specify one and the alias has no `/`. e.g. `opencode/` turns `gpt-5.5` into `opencode/gpt-5.5`. |
| `max_tokens_field` | string | `max_tokens` | Field used to send the token cap (`max_tokens` or `max_completion_tokens`). |
| `extra_headers` | table | `{}` | Extra HTTP headers sent on every upstream request. |

### Provider keys

For a provider named `go`, the bridge reads **`BRIDGE_PROVIDERS_GO_API_KEY`** at startup and,
if present, it overrides any inline `api_key`. The pattern is
`BRIDGE_PROVIDERS_<UPPERCASE_NAME>_API_KEY`:

```bash
export BRIDGE_PROVIDERS_ZEN_API_KEY=<your-zen-key>    # for [providers.zen]
export BRIDGE_PROVIDERS_GO_API_KEY=<your-go-key>      # for [providers.go]
```

### The two OpenCode endpoints

```toml
# Standard Zen gateway — OpenAI/Anthropic/Gemini models, ids of the form opencode/<id>.
[providers.zen]
wire = "openai-chat"
base_url = "https://opencode.ai/zen/v1"
model_prefix = "opencode/"

# The "go" package — separate key + endpoint, ids passed through as-is.
[providers.go]
wire = "openai-chat"
base_url = "https://opencode.ai/zen/go/v1"
model_prefix = ""
```

**Models on the `go` package** (`GET /zen/go/v1/models`):

| Family | Model ids |
|---|---|
| DeepSeek | `deepseek-v4-pro`, `deepseek-v4-flash` |
| GLM | `glm-5.1`, `glm-5` |
| Kimi | `kimi-k2.6`, `kimi-k2.5` |
| MiniMax | `minimax-m3`, `minimax-m2.7`, `minimax-m2.5` |
| Qwen | `qwen3.7-max`, `qwen3.7-plus`, `qwen3.6-plus`, `qwen3.5-plus` |
| MiMo | `mimo-v2-pro`, `mimo-v2-omni`, `mimo-v2.5-pro`, `mimo-v2.5` |
| Other | `hy3-preview` |

The list can change — query the endpoint to see the current set:

```bash
curl -s https://opencode.ai/zen/go/v1/models -H "Authorization: Bearer $BRIDGE_PROVIDERS_GO_API_KEY"
```

> **Thinking models** (e.g. `deepseek-v4-pro`) work without extra config: the bridge
> carries each turn's `reasoning_content` back to the model across multi-step tool
> conversations, which DeepSeek's thinking mode requires.

## Routes

A `[[routes]]` entry maps the model name a client sends (`alias`) to a concrete
provider + upstream model.

| Key | Meaning |
|---|---|
| `alias` | The `model` value the client sends. |
| `provider` | Which `[providers.<name>]` to use. |
| `model` | The upstream model id to send. |

```toml
# Codex sends model = "gpt-5.5"; serve it from the go package's deepseek-v4-pro.
[[routes]]
alias = "gpt-5.5"
provider = "go"
model = "deepseek-v4-pro"

[[routes]]
alias = "fast"
provider = "go"
model = "deepseek-v4-flash"
```

### How a model name is resolved

1. If a `[[routes]]` entry's `alias` matches the requested model exactly → use its
   `provider` + `model`.
2. Otherwise → use `default_provider`; the upstream model is the requested name with the
   provider's `model_prefix` applied (only if the name has no `/`).
3. If neither applies (no route, no default) → `400 unknown model`.

This means you can switch which upstream model backs Codex by editing a route's `model`,
without touching Codex itself.

## Pointing Codex at the bridge

In `~/.codex/config.toml`:

```toml
model_provider = "bridge"
model = "gpt-5.5"            # must match a route alias (or default-provider model)

[model_providers.bridge]
name = "bridge"
base_url = "http://127.0.0.1:8282/v1"
wire_api = "responses"      # the only protocol Codex supports
env_key = "BRIDGE_KEY"      # value is ignored unless `auth_token` is set in bridge.toml
```

Run Codex with `BRIDGE_KEY=<auth_token> codex` (any value if `auth_token` is unset).

## Pointing Claude Code at the bridge

Claude Code speaks the Anthropic **Messages API**; point it at the bridge with environment
variables:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8282
export ANTHROPIC_API_KEY=<your bridge auth_token>   # sent as x-api-key; any value if auth_token is unset
export ANTHROPIC_MODEL=gpt-5.5                       # must match a [[routes]] alias
claude
```

The bridge accepts the key via either `x-api-key` (default) or `Authorization: Bearer`
(`ANTHROPIC_AUTH_TOKEN`), checked against `auth_token`. The model name (`ANTHROPIC_MODEL`)
is resolved through `[[routes]]` exactly like any other client. Thinking models work:
DeepSeek `reasoning_content` is surfaced as Anthropic `thinking` blocks and carried back
across turns.

## Endpoints exposed by the bridge

| Endpoint | Purpose |
|---|---|
| `POST /v1/responses` | OpenAI Responses API — for Codex. |
| `POST /v1/messages` | Anthropic Messages API — for Claude Code (`ANTHROPIC_BASE_URL`). |
| `POST /v1/chat/completions` | OpenAI Chat Completions — verbatim passthrough. |
| `GET /v1/models` | Lists configured route aliases. |
| `GET /health` | Liveness check (returns `ok`). |

## Full example

```toml
listen = "127.0.0.1:8282"
default_provider = "zen"
# auth_token = "a-long-random-string"

[providers.zen]
wire = "openai-chat"
base_url = "https://opencode.ai/zen/v1"
model_prefix = "opencode/"

[providers.go]
wire = "openai-chat"
base_url = "https://opencode.ai/zen/go/v1"
model_prefix = ""

[[routes]]
alias = "gpt-5.5"
provider = "go"
model = "deepseek-v4-pro"
```
