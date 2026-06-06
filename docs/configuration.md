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
- `--db <path>` overrides the SQLite database path (default: the `database` key).
- Secrets can be supplied by environment variable instead of being written into the
  file (see [Provider keys](#provider-keys)). **Keep `bridge.toml` out of git** — it is
  already in `.gitignore` because it holds API keys.

## Provider store (SQLite)

`[providers.<name>]` and `[[routes]]` are **not re-read from `bridge.toml` on every run** —
they live in a SQLite database (`bridge.db` by default; set `database` or `--db`):

- **First run** (empty DB): the `[providers]` / `[[routes]]` in `bridge.toml` are imported
  into the DB once (seed).
- **After that the DB is the source of truth.** Editing those two sections in `bridge.toml`
  has no effect — edit the DB directly, or run with `--reseed` to wipe both tables and
  re-import from `bridge.toml`.
- `BRIDGE_PROVIDERS_<NAME>_API_KEY` env vars are applied **after** the DB load, so a key
  supplied only via env is never persisted to the DB.
- The other top-level keys (`listen`, `default_provider`, `auth_token`, `database`) are
  always read from `bridge.toml`, never the DB.

`bridge.db` holds provider API keys, so it is gitignored like `bridge.toml`.

## Top-level keys

| Key | Type | Default | Meaning |
|---|---|---|---|
| `listen` | string | `127.0.0.1:8282` | Address the bridge binds to. |
| `database` | string | `bridge.db` | SQLite file holding providers + routes (see [Provider store](#provider-store-sqlite)). Override with `--db`. |
| `default_provider` | string | — | Provider used when no `[[routes]]` entry matches. Optional, but without it any unrouted model name is a 400. |
| `auth_token` | string | — | If set, clients must send the token as `Authorization: Bearer <token>` or `x-api-key: <token>` (Claude Code). If unset, any/no token is accepted. Also gates the admin API. |
| `cost_tracking` | bool | `false` | Master switch for [usage tracking](#usage-tracking-cost--count--token-windows). Off = no usage recorded/exposed and no usage-based failover, at zero overhead (429 failover is unaffected). Toggle at runtime on the admin page / `POST /admin/api/usage`. |
| `[providers.<name>]` | table | — | Upstream providers — **seed the SQLite store** on first run (see above). |
| `[[routes]]` | array | — | Model alias → provider/model mappings — seed the store on first run. |

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
| `probe_script` | string | — | Path to a Lua quota probe (see [Provider watcher](#provider-watcher-quota--availability--failover)). Absent = availability-ping only. |
| `probe_enabled` | bool | on if `probe_script` set | Master switch for monitoring this provider. |
| `probe_interval_secs` | int | `300` | Seconds between probes. |
| `quota_min` | float | — | Below this `quota_remaining` the provider counts as exhausted (for failover). |
| `usage` | array | `[]` | Typed usage limit windows — see [Usage tracking](#usage-tracking-cost--count--token-windows). Empty = not usage-tracked. |
| `cost_windows` / `model_prices` | — | — | **Legacy** billing shorthand; folded into a `usage` `billing` spec at load. Prefer `usage`. |

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
| `fallback` | Ordered `{ provider, model }` list tried (in order) when the primary is unavailable / quota-exhausted (see [Provider watcher](#provider-watcher-quota--availability--failover)). |

```toml
# Codex sends model = "gpt-5.5"; serve it from the go package's deepseek-v4-pro,
# falling back to zen if go is down or out of quota.
[[routes]]
alias = "gpt-5.5"
provider = "go"
model = "deepseek-v4-pro"
fallback = [{ provider = "zen", model = "gpt-5.5" }]

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

## Provider watcher (quota + availability + failover)

A background watcher tracks each provider's **availability** and (where the vendor exposes
it) **remaining quota**, persists it to the DB, and serves it at `GET /v1/providers`. The
router uses it to **fail over** away from a down or exhausted provider.

### Probes are Lua scripts

Quota APIs differ per vendor, so the bridge never hardcodes them — each provider points at a
Lua script (`probe_script`) that drives the HTTP call and returns the result. Scripts get:

- `ctx` — `{ name, base_url, api_key, extra_headers, wire }`.
- `http{ url, method = "GET", headers = {}, body = nil }` → `{ status, body }`.
- `json_decode(str)` → table, `json_encode(table)` → string.

and return `{ ok = bool, remaining?, used?, limit?, note? }`. A script error or a missing
return is recorded as unavailable. Example probes live in [`probes/`](../probes):
`opencode-zen.lua` (availability via `/models`; Zen has no quota endpoint) and
`generic-credits.lua` (a template for vendors that expose a credits endpoint).

```lua
-- probes/opencode-zen.lua
local resp = http {
    url = ctx.base_url .. "/models",
    headers = { authorization = "Bearer " .. (ctx.api_key or "") },
}
return { ok = resp.status == 200, note = "GET /models -> " .. resp.status }
```

Providers with **no** `probe_script` (but `probe_enabled = true`) get a lightweight
connectivity ping instead: any HTTP response = available, a connection error = unavailable.

### Scheduling & switch

Each enabled provider is probed at startup and every `probe_interval_secs` (default 300).
`probe_enabled` is the master switch (defaults on when a `probe_script` is set). Probing is
periodic and out-of-band — it never adds latency to a request.

### Failover (proactive + reactive)

A request resolves to a **candidate chain** — the route's primary then its `fallback` list
(in order) — reordered so watcher-usable providers come first. Two layers then apply:

- **Proactive** (watcher status): a candidate is ranked last when its status is
  `available = false`, or `quota_remaining` is known and below the provider's `quota_min`.
  Providers that aren't monitored (no status yet) are assumed usable.
- **Reactive** (this request): the bridge tries candidates in order and, if one **fails at
  call time**, advances to the next. It retries on connection failure / timeout, **5xx**,
  **429** (rate-limit), and **401/402** (auth/quota) — but **not** other 4xx (a bad request
  won't be fixed by retrying elsewhere). For **streaming**, the upstream response headers
  arrive before any body byte, so a failure is caught *before* anything is sent to the client
  — once bytes start flowing the response is committed (no retry). On a reactive failure the
  provider is marked `available = false` immediately, and (if it has a probe) a one-shot
  re-probe writes the corrected status to map + DB so a transient blip self-heals in seconds.

Each candidate is attempted at most once, in order — total attempts ≤ chain length (no
backoff loop, no same-provider retry). If every candidate fails, the last error is returned.

Connection/headers timeouts: the upstream client uses a 10s connect timeout, a 30s
response-**headers** timeout for streaming (the body is never time-capped, so long streams
aren't cut), and a 60s whole-request timeout for non-streaming calls.

### Status endpoint

`GET /v1/providers` returns each provider's `available`, `quota_remaining/used/limit`,
`quota_min`, `last_checked` (epoch secs), `last_ok`, `error`, `note`, and `usage` (per-type
windows, see below). The response also carries a top-level `cost_tracking` flag.

## Usage tracking (cost / count / token windows)

Separately from the watcher's vendor-reported quota, the bridge can **accumulate usage it
observes** into rolling limit windows and fail over before a window is exhausted. This is
**off by default** — enable it with the top-level `cost_tracking = true`, or toggle it at
runtime on the admin page / `POST /admin/api/usage {"enabled": true}`. When off, the whole
subsystem short-circuits at zero overhead; the **reactive 429 failover is independent and
always on**.

### Usage types

Each provider's `usage` is a list of typed specs (`usage_type`):

| Type | Unit | Amount recorded per request | Notes |
|---|---|---|---|
| `billing` | `usd` | the response's `cost`, or — when that is `$0`/absent — a `tokens × model_prices` estimate | for subscription plans (e.g. OpenCode Go) that report `cost = $0` |
| `count` | `requests` | `1` | cap request volume |
| `token` | `tokens` | `prompt_tokens + completion_tokens` | cap token throughput |

Each spec has rolling `windows` (`{label, window_secs, limit}`); `limit` is in the type's
unit. `spent = Σ amount in the last window_secs`, `remaining = limit − spent`. A provider may
list several specs (e.g. a `$`-cap and a request-count cap at once).

```toml
[[providers.go.usage]]
usage_type = "billing"
windows = [
    { label = "5h", window_secs = 18000, limit = 12.0 },   # OpenCode Go: $12 / 5h
    { label = "7d", window_secs = 604800, limit = 30.0 },
    { label = "30d", window_secs = 2592000, limit = 60.0 },
]

[providers.go.usage.model_prices]   # $/1M tokens — drives the billing estimate when cost=$0
"deepseek-v4-pro" = { input = 0.40, output = 1.60 }
# … one entry per model id you route to …

[[providers.go.usage]]
usage_type = "count"
windows = [{ label = "1d", window_secs = 86400, limit = 500.0 }]   # 500 requests/day
```

> **Legacy shorthand:** a provider's older `cost_windows = [...]` + `[providers.<name>.model_prices]`
> still work — they fold into one `billing` spec at load. New configs should use `usage`.

### Exposure + failover

- **Exposed** at `GET /v1/providers` and `GET /admin/api/providers` as `usage: [{usage_type,
  unit, windows: [{label, limit, spent, remaining, reset_in_secs}]}]` (config-only when
  tracking is off). The admin page renders one bar section per type.
- **Failover:** a provider whose **any** window (any unit) has `remaining ≤ 0` is demoted like
  a watcher-down provider — tried only as a last resort — so traffic moves to a fallback
  *before* the vendor returns 429.

Events persist to the SQLite `usage_events` table (kept ~31 days) so windows survive a restart.

## Admin web UI

`GET /` (and `/admin`) serves a single-page dashboard embedded in the binary (no runtime CDN —
works offline). It does full CRUD over the SQLite-backed providers + routes, shows live status
pills + quota/usage bars, and toggles usage tracking — all gated by `auth_token` (the page
prompts for it once and keeps it in browser `localStorage`). Backing REST API:

| Endpoint | Purpose |
|---|---|
| `GET/POST /admin/api/providers` | List (api_key masked) / create a provider. |
| `PUT/DELETE /admin/api/providers/:name` | Update (blank `api_key` keeps the stored one) / delete (cascades its routes). |
| `GET/POST /admin/api/routes` | List / create a route. |
| `PUT/DELETE /admin/api/routes/:alias` | Update / delete a route. |
| `GET/POST /admin/api/usage` | Read / set the usage-tracking on-off switch (runtime; the persistent default is `cost_tracking`). |

Every write rebuilds the live config and restarts the watcher in place — **no process restart**.
(Edits go to the DB, which is authoritative; `bridge.toml` only seeds an empty DB or re-seeds via
`--reseed`.)

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
| `GET /v1/providers` | Watcher status per provider (availability + quota + usage). |
| `GET /health` | Liveness check (returns `ok`). |
| `GET /` · `GET /admin` | [Admin web UI](#admin-web-ui) (dashboard). |
| `/admin/api/providers[/:name]` · `/admin/api/routes[/:alias]` | Provider/route CRUD (reuse `auth_token`). |
| `GET/POST /admin/api/usage` | Usage-tracking on/off toggle. |

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
