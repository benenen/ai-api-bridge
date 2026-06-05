# ai-api-bridge — Design Spec

**Date:** 2026-06-05
**Status:** Approved (design); pending implementation plan
**Author:** brainstorming session

## 1. Purpose

A local **format-translating AI API proxy**. It lets a client tool that speaks one
wire format use a provider/account that speaks a different wire format, without the
client knowing.

Concrete driving case: use an **OpenCode Zen** account (a plain bearer API key, stored
in opencode as the `opencode-go` provider) inside the **OpenAI Codex CLI**.

- Codex (since Feb 2026) speaks **only** the OpenAI **Responses API** (`wire_api = "responses"`).
- OpenCode Zen exposes OpenAI **Chat Completions** (`/zen/v1/chat/completions`),
  Anthropic Messages (`/zen/v1/messages`), and an OpenAI Responses endpoint
  (`/zen/v1/responses`), over 40+ models (`gpt-5.x`, Claude, Gemini, …), authenticated
  with a bearer key. Zen model ids use the `opencode/<model>` form.

The bridge presents a Responses API endpoint to Codex, translates each request into a
Chat Completions call to Zen using the configured Zen key, and translates the streaming
response back into Responses API events.

## 2. Goals / Non-goals

### Goals (v1)
- A general translation architecture (any inbound format ↔ any outbound provider) routed
  through a single provider-neutral canonical representation — **no N×N translators**.
- One fully working vertical slice (the tracer bullet): **Responses inbound → Chat
  Completions outbound (Zen)**, streaming and non-streaming, including text, reasoning,
  tool calls, and usage.
- Config-file driven: providers (wire format, base_url, api_key, model rewrite) and model
  routes live in `bridge.toml`. The Zen key lives here (user's chosen key source).
- Deterministic tests that do not touch the network (golden translation tests +
  full-pipeline test against a mock upstream).

### Non-goals (v1) — seams only, additive later
- Anthropic inbound/outbound; OpenAI-Responses **outbound**.
- Multi-key load balancing / failover; response caching; a web UI.
- Auto-reading opencode's `auth.json` (user chose config-file as key source).
- Cross-turn reasoning-token reuse (see §11 Known limitations).

## 3. Architecture — translate through a canonical IR

Everything translates to/from a provider-neutral middle representation; wire formats never
translate directly to each other.

```
Codex                         BRIDGE                                    Zen
 Responses req ─► [inbound: Responses.parse] ─► CanonicalRequest
                                                     │
                                              [router: model→provider]
                                                     ▼
                              CanonicalRequest ─► [outbound: Chat.build] ─► POST /chat/completions
                                                                                    │
 Responses SSE ◄─ [inbound: Responses.emit] ◄─ Stream<CanonicalEvent> ◄─ [outbound: Chat.parse] ◄─ CC SSE
```

A **wire format** is an `enum { OpenAIResponses, OpenAIChat, AnthropicMessages }`. Each
format may provide up to four functions:

| Direction | Functions | Role |
|---|---|---|
| Inbound (server side) | `parse_request`, `emit_stream` | accept a client request, emit the client's response stream |
| Outbound (client side) | `build_request`, `parse_stream` | call an upstream provider, parse its response stream |

v1 implements **Responses inbound** (`parse_request` + `emit_stream`) and **Chat outbound**
(`build_request` + `parse_stream`). Other cells are stubs returning a clear
"not implemented" error.

Dispatch is `match` over the enum — not async trait objects — keeping streaming code simple.

## 4. Canonical data model (`canonical.rs`)

Approximate shapes (final field names settled during TDD):

```rust
struct CanonicalRequest {
    model: String,                 // client-facing alias, pre-routing
    system: Option<String>,        // merged instructions/system/developer text
    messages: Vec<Message>,
    tools: Vec<ToolDef>,
    tool_choice: ToolChoice,       // Auto | None | Required | Function(name)
    temperature: Option<f32>,
    top_p: Option<f32>,
    max_output_tokens: Option<u32>,
    reasoning_effort: Option<ReasoningEffort>, // Minimal|Low|Medium|High|XHigh
    parallel_tool_calls: Option<bool>,
    stream: bool,
}

enum Message {
    User(Vec<ContentPart>),
    Assistant { text: Option<String>, tool_calls: Vec<ToolCall> },
    Tool { call_id: String, output: String },
}
// ContentPart::Text { text }  (images deferred)

struct ToolDef { name, description: Option<String>, parameters: serde_json::Value }
struct ToolCall { call_id, name, arguments: String /* raw JSON */ }

enum CanonicalEvent {
    Created { response_id: String, model: String },
    ReasoningDelta { text: String },
    TextDelta { text: String },
    ToolCallStart { index: u32, call_id: String, name: String },
    ToolCallArgsDelta { index: u32, delta: String },
    ToolCallDone { index: u32 },
    Usage { input_tokens: u32, output_tokens: u32, total_tokens: u32 },
    Completed,
    Error { message: String, kind: ErrorKind },
}
```

The `CanonicalEvent` stream is the spine both wire formats serialize to/from.

## 5. Wire format details

### 5.1 Responses **inbound** (`wire/responses.rs`)

**`parse_request(json) -> CanonicalRequest`**
- `instructions` (+ any `developer`/`system` role input items) → `system`.
- `input`: string → one user message; array → map items:
  - `{type:"message", role, content:[{type:"input_text"|"output_text", text}]}` → User/Assistant message.
  - `{type:"function_call", call_id, name, arguments}` → fold into preceding Assistant message's `tool_calls`.
  - `{type:"function_call_output", call_id, output}` → `Message::Tool`.
  - `{type:"reasoning", …}` → dropped (not forwarded upstream; see §11).
- `tools` (Responses **flattens** function fields: `{type:"function", name, description, parameters}`) → `ToolDef`.
- `tool_choice`, `temperature`, `top_p`, `max_output_tokens`, `parallel_tool_calls` → direct.
- `reasoning.effort` → `reasoning_effort`.
- `stream` → `stream`. `store`, `previous_response_id`, `include`, `metadata` → ignored.

**`emit_stream(Stream<CanonicalEvent>) -> Sse`** — emit the Responses event sequence. Each
SSE frame carries an `event:` name and a `data:` JSON object that also contains `type`.

Ordering of output items (each gets an incrementing `output_index`): reasoning item (if
any) → assistant message → function_call item(s).

Event sequence:
1. `response.created` (response: id, `status:"in_progress"`, model, `output:[]`).
2. *(reasoning, if any)* `response.output_item.added` (item `type:"reasoning"`) →
   `response.reasoning_summary_text.delta`* → `…done` → `response.output_item.done`.
3. *(assistant text)* `response.output_item.added` (item `type:"message"`, role assistant) →
   `response.content_part.added` (`output_text`) → `response.output_text.delta`* →
   `response.output_text.done` → `response.content_part.done` → `response.output_item.done`.
4. *(each tool call)* `response.output_item.added` (item `type:"function_call"`, with
   `call_id`,`name`) → `response.function_call_arguments.delta`* →
   `response.function_call_arguments.done` → `response.output_item.done`.
5. `response.completed` (full `output` array, `status:"completed"`, `usage:{input_tokens,
   output_tokens, total_tokens}`).

On error mid-stream: emit `response.failed` (or `error`) with translated message, then end.

**Non-streaming** (`stream:false`): fold the canonical events into a single `response`
object and return it as JSON (HTTP 200).

IDs generated by the bridge: `resp_<uuid>`, `msg_<uuid>`, `fc_<uuid>`; tool `call_id`
preserved from upstream when present, else generated.

### 5.2 Chat Completions **outbound** (`wire/chat.rs`)

**`build_request(CanonicalRequest, upstream_model) -> json`**
- `model` = routed upstream model (e.g. `opencode/gpt-5.5`).
- `messages`: `system` → leading `{role:"system"}`; User → `{role:"user", content}`;
  Assistant → `{role:"assistant", content?, tool_calls:[{id, type:"function",
  function:{name, arguments}}]}`; Tool → `{role:"tool", tool_call_id, content}`.
- `tools`: **nested** form `{type:"function", function:{name, description, parameters}}`.
- `tool_choice`, `temperature`, `top_p`, `parallel_tool_calls` → direct.
- `max_output_tokens` → `max_tokens` (field name configurable per provider:
  `max_tokens` | `max_completion_tokens`).
- `reasoning_effort` → `reasoning_effort` (pass-through; configurable strip/remap, e.g.
  `xhigh`→`high`, on upstream rejection).
- When streaming: `stream:true`, `stream_options:{include_usage:true}`.

**`parse_stream(byte_stream) -> Stream<CanonicalEvent>`** — decode upstream SSE; per chunk:
- `choices[0].delta.content` → `TextDelta`.
- `choices[0].delta.reasoning_content` / `.reasoning` → `ReasoningDelta`.
- `choices[0].delta.tool_calls[]`: first frame with `id`/`function.name` →
  `ToolCallStart{index}`; subsequent `function.arguments` fragments → `ToolCallArgsDelta`;
  `finish_reason:"tool_calls"` → `ToolCallDone` for open calls.
- final `usage` → `Usage`. `data: [DONE]` → `Completed`.
- A non-2xx upstream status, or an upstream `{error:…}` body, → `Error`.

## 6. Router & model mapping (`router.rs`)

`resolve(model_alias) -> (provider_id, upstream_model)`:
1. Exact match in `[[routes]]` wins.
2. Else default provider; apply that provider's `model_prefix` if the alias lacks it
   (`gpt-5.5` → `opencode/gpt-5.5`).
3. Unknown alias with no default → 400 with a clear error.

## 7. Config (`config.rs`) — `bridge.toml`

```toml
listen = "127.0.0.1:8282"
default_provider = "zen"
# auth_token = "..."   # optional bearer the bridge requires from clients; omitted = accept any

[providers.zen]
wire = "openai-chat"                  # outbound wire format
base_url = "https://opencode.ai/zen/v1"
# Zen key — preferred via env BRIDGE_PROVIDERS_ZEN_API_KEY, or inline (`api_key`)
model_prefix = "opencode/"            # applied when alias lacks a "/"
max_tokens_field = "max_tokens"       # or "max_completion_tokens"
# extra_headers = { "x-foo" = "bar" }

[[routes]]                            # optional explicit overrides
alias = "gpt-5.5"
provider = "zen"
model = "opencode/gpt-5.5"
```

Loading: `toml` + serde; env overrides for secrets (e.g. `BRIDGE_PROVIDERS_ZEN_API_KEY`)
so the key need not be committed. CLI flag `--config <path>` (default `./bridge.toml`),
`--listen` override.

## 8. HTTP server (`server.rs`, axum)

Endpoints:
- `POST /v1/responses` — Responses inbound (**primary, v1**).
- `POST /v1/chat/completions` — Chat inbound (v1 follow-on, near-free via same IR; lets
  non-Codex tools use the bridge).
- `POST /v1/messages` — Anthropic inbound (stub → 501).
- `GET /v1/models` — list configured aliases/routes (OpenAI-shaped).
- `GET /health` — liveness.

Client auth: if `auth_token` set, require matching `Authorization: Bearer`; else accept any
(the value Codex sends via `env_key` is ignored). Upstream auth: provider `api_key` as
`Authorization: Bearer` to Zen.

## 9. Upstream client (`upstream.rs`) + SSE (`sse.rs`)

- `reqwest` (rustls, streaming) issues the upstream POST; response body exposed as a byte
  stream.
- `sse.rs`: a small SSE decoder turning the upstream byte stream into `data:` frames
  (handles multi-line `data:`, comments, `[DONE]`); plus helpers to build axum
  `sse::Event`s for the inbound side.
- Streaming end-to-end: upstream bytes → SSE frames → `parse_stream` (canonical) →
  `emit_stream` (Responses SSE) → axum `Sse` to Codex. Backpressure via the async stream.

## 10. Error handling (`error.rs`)

| Failure | Result |
|---|---|
| Malformed client request | 400, error JSON in the client's format |
| Unknown model / no route | 400 |
| Upstream non-2xx / error body | translated error → SSE `response.failed`/`error` when streaming, else mapped HTTP status + JSON |
| Upstream connect/timeout | 502 / 504 |
| Unimplemented wire cell | 501 |

A single `BridgeError` type maps to both an HTTP status and a client-format error body.

## 11. Known limitations (accepted for v1)

- **Reasoning continuity:** Responses-with-`store:false` normally round-trips encrypted
  reasoning across turns. Chat Completions is stateless and does not, so the bridge drops
  inbound `reasoning` items and does not echo encrypted reasoning. Each turn still works;
  there's just no cross-turn reasoning-token reuse. Reasoning *summary* deltas are still
  surfaced when the upstream streams them.
- **Images / non-text content parts** are deferred.
- `previous_response_id` (server-side conversation state) is unsupported; the bridge is
  stateless. Fine for Codex (`disable_response_storage = true`).

## 12. Testing strategy (TDD)

- **Golden translation tests** (pure, no IO):
  - fixture Codex Responses request JSON → asserted Chat Completions request JSON
    (messages, tools, tool results, reasoning_effort, max_tokens mapping).
  - recorded Zen Chat Completions SSE → asserted `CanonicalEvent` sequence.
  - `CanonicalEvent` sequence → asserted Responses SSE frames (text, reasoning, tool
    calls, usage, completion ordering).
- **Full-pipeline integration test:** a local **mock upstream** axum server replays a
  recorded Zen stream; a Codex-shaped Responses request is sent through the real bridge;
  assert the emitted Responses SSE. Deterministic, offline.
- **Error-path tests:** upstream 401/429/500, malformed request, unknown model.
- **Manual smoke (last):** real Codex → bridge → Zen.

## 13. Dependencies

tokio (rt-multi-thread, macros), axum (+ sse), reqwest (rustls-tls, stream, json),
serde / serde_json, toml, futures-util, bytes, tokio-stream, uuid, thiserror, anyhow,
tracing, tracing-subscriber (env-filter), clap (derive). Dev: an in-process axum mock
upstream (no extra crate needed).

## 14. Module layout

```
src/
  main.rs          CLI parse, load config, start server
  config.rs        Config structs + loading (toml + env), validation
  server.rs        axum router + endpoint handlers + client auth
  router.rs        model alias → (provider, upstream model)
  canonical.rs     CanonicalRequest / Message / ToolDef / ToolCall / CanonicalEvent
  upstream.rs      reqwest client → upstream SSE byte stream
  sse.rs           upstream SSE decoder + inbound Event helpers
  error.rs         BridgeError → (HTTP status, wire error body)
  wire/
    mod.rs         WireFormat enum + dispatch
    responses.rs   parse_request + emit_stream (inbound); outbound = todo
    chat.rs        build_request + parse_stream (outbound); inbound = follow-on
    anthropic.rs   stub (501)
tests/
  pipeline.rs      mock-upstream end-to-end
  fixtures/        recorded request/stream JSON
docs/superpowers/specs/2026-06-05-ai-api-bridge-design.md
README.md          quickstart + Codex wiring
CLAUDE.md          architecture notes for future Claude sessions (written at the end)
```

## 15. Implementation milestones (tracer-bullet order)

1. **Scaffold** — deps, `config.rs`, axum server, `GET /health`. Runs.
2. **Canonical + Responses parse** — `canonical.rs`, `responses::parse_request` (+ tests).
3. **Chat build** — `chat::build_request` from canonical (+ tests).
4. **Chat parse** — `chat::parse_stream` over recorded SSE → canonical events (+ tests).
5. **Responses emit** — `responses::emit_stream` from canonical events (+ golden tests).
6. **Wire it** — `POST /v1/responses` handler, router, upstream client; mock-upstream
   integration test (streaming + non-streaming) green.
7. **Robustness** — error mapping, `/v1/models`, logging, env-override for the key.
8. **Follow-on** — `/v1/chat/completions` inbound; anthropic stubs.
9. **Ship** — real Codex smoke test; write README + CLAUDE.md.

## 16. Codex integration (end-state)

`~/.codex/config.toml`:
```toml
model_provider = "bridge"
model = "gpt-5.5"

[model_providers.bridge]
name = "bridge"
base_url = "http://127.0.0.1:8282/v1"
wire_api = "responses"
env_key = "BRIDGE_KEY"   # value ignored unless bridge auth_token is set
```
Run `BRIDGE_KEY=x ai-api-bridge --config bridge.toml`, point Codex at it, done.
