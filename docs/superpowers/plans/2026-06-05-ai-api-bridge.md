# ai-api-bridge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A local HTTP proxy that lets the Codex CLI (which speaks the OpenAI Responses API) drive an OpenCode Zen account (which speaks OpenAI Chat Completions), by translating requests/responses through a provider-neutral canonical representation.

**Architecture:** Inbound wire format → `CanonicalRequest` → router picks a provider → outbound wire format builds the upstream call → upstream streaming response → `CanonicalEvent` stream → inbound wire format serializes the client's response. v1 implements Responses-inbound + Chat-outbound; other cells are stubs. Dispatch is `match` over enums, not async trait objects.

**Tech Stack:** Rust 2024 (rustc 1.96), tokio, axum (+ SSE), reqwest (rustls, streaming), serde / serde_json, toml, async-stream, futures-util, uuid, thiserror, tracing, clap.

**Spec:** `docs/superpowers/specs/2026-06-05-ai-api-bridge-design.md`

---

## File Structure

```
Cargo.toml                  deps
src/main.rs                 CLI parse, load config, init tracing, start server
src/config.rs               Config/Provider/Route structs, TOML load + env override
src/error.rs                BridgeError + IntoResponse mapping
src/canonical.rs            CanonicalRequest/Message/ToolDef/ToolCall/CanonicalEvent + enums
src/router.rs               resolve(alias) -> (provider, upstream_model)
src/upstream.rs             reqwest client: post_stream (SSE) + post_json
src/sse.rs                  SseDecoder: bytes -> data frames / [DONE]
src/wire/mod.rs             module exports
src/wire/responses.rs       parse_request (inbound) + ResponsesEmitter (inbound)
src/wire/chat.rs            build_request + ChatStreamParser + completion_to_events (outbound)
src/wire/anthropic.rs       stub (501)
src/server.rs               axum app, AppState, handlers, streaming pipeline, auth
tests/pipeline.rs           end-to-end test against an in-process mock upstream
tests/fixtures/             recorded JSON request/response samples
README.md                   quickstart + Codex wiring
CLAUDE.md                   architecture notes for future Claude sessions (final task)
```

Each `src/wire/*.rs` owns one wire format's translation in both directions. `canonical.rs` is the contract every other module depends on, so it is built first (Task 2).

**Convention for every task:** run `cargo test` after each implementation step; commit only when green. Test names are exact so steps can name the command.

---

## Task 1: Scaffold — deps, config, health server

**Files:**
- Modify: `Cargo.toml`
- Create: `src/main.rs` (replace the hello-world), `src/config.rs`, `src/server.rs`, `src/error.rs`

- [ ] **Step 1: Set dependencies in `Cargo.toml`**

```toml
[package]
name = "ai-api-bridge"
version = "0.1.0"
edition = "2024"

[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "signal"] }
axum = "0.7"
reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
async-stream = "0.3"
futures-util = "0.3"
bytes = "1"
uuid = { version = "1", features = ["v4"] }
thiserror = "1"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
clap = { version = "4", features = ["derive"] }

[dev-dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "time"] }
```

- [ ] **Step 2: Write the failing config test**

Create `src/config.rs`:

```rust
//! Bridge configuration: providers (outbound targets) + model routes.

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    pub default_provider: Option<String>,
    pub auth_token: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, Provider>,
    #[serde(default)]
    pub routes: Vec<Route>,
}

fn default_listen() -> String {
    "127.0.0.1:8282".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Provider {
    pub wire: WireName,
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub model_prefix: Option<String>,
    #[serde(default = "default_max_tokens_field")]
    pub max_tokens_field: String,
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
}

fn default_max_tokens_field() -> String {
    "max_tokens".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WireName {
    OpenaiChat,
    OpenaiResponses,
    AnthropicMessages,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    pub alias: String,
    pub provider: String,
    pub model: String,
}

impl Config {
    pub fn from_toml(text: &str) -> anyhow::Result<Config> {
        let mut cfg: Config = toml::from_str(text)?;
        cfg.apply_env_overrides();
        Ok(cfg)
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading config {}: {e}", path.display()))?;
        Config::from_toml(&text)
    }

    fn apply_env_overrides(&mut self) {
        for (name, provider) in self.providers.iter_mut() {
            let key = format!("BRIDGE_PROVIDERS_{}_API_KEY", name.to_uppercase());
            if let Ok(val) = std::env::var(&key) {
                provider.api_key = Some(val);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_and_route() {
        let toml = r#"
listen = "127.0.0.1:9000"
default_provider = "zen"

[providers.zen]
wire = "openai-chat"
base_url = "https://opencode.ai/zen/v1"
model_prefix = "opencode/"

[[routes]]
alias = "gpt-5.5"
provider = "zen"
model = "opencode/gpt-5.5"
"#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:9000");
        assert_eq!(cfg.default_provider.as_deref(), Some("zen"));
        let zen = cfg.providers.get("zen").unwrap();
        assert_eq!(zen.wire, WireName::OpenaiChat);
        assert_eq!(zen.model_prefix.as_deref(), Some("opencode/"));
        assert_eq!(zen.max_tokens_field, "max_tokens"); // default
        assert_eq!(cfg.routes.len(), 1);
        assert_eq!(cfg.routes[0].model, "opencode/gpt-5.5");
    }

    #[test]
    fn listen_defaults() {
        let cfg = Config::from_toml("").unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:8282");
    }
}
```

- [ ] **Step 3: Create minimal `src/error.rs`, `src/server.rs`, `src/main.rs` so it compiles**

`src/error.rs`:

```rust
//! Bridge error type; maps to an HTTP status + a client-format error body.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("unknown model: {0}")]
    UnknownModel(String),
    #[error("upstream error ({status}): {message}")]
    Upstream { status: u16, message: String },
    #[error("upstream unreachable: {0}")]
    UpstreamUnreachable(String),
    #[error("not implemented: {0}")]
    NotImplemented(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl BridgeError {
    pub fn http_status(&self) -> StatusCode {
        match self {
            BridgeError::BadRequest(_) | BridgeError::UnknownModel(_) => StatusCode::BAD_REQUEST,
            BridgeError::Upstream { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            BridgeError::UpstreamUnreachable(_) => StatusCode::BAD_GATEWAY,
            BridgeError::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            BridgeError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for BridgeError {
    fn into_response(self) -> Response {
        let status = self.http_status();
        let body = json!({ "error": { "message": self.to_string(), "type": "bridge_error" } });
        (status, Json(body)).into_response()
    }
}
```

`src/server.rs`:

```rust
//! axum app + handlers.

use std::sync::Arc;

use axum::routing::get;
use axum::Router;

use crate::config::Config;

pub struct AppState {
    pub config: Config,
}

pub fn build_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
```

`src/main.rs`:

```rust
mod canonical;
mod config;
mod error;
mod router;
mod server;
mod sse;
mod upstream;
mod wire;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

use crate::config::Config;
use crate::server::{build_app, AppState};

#[derive(Parser, Debug)]
#[command(name = "ai-api-bridge")]
struct Cli {
    /// Path to the bridge config file
    #[arg(long, default_value = "bridge.toml")]
    config: PathBuf,
    /// Override the listen address (host:port)
    #[arg(long)]
    listen: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ai_api_bridge=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let mut config = Config::load(&cli.config)?;
    if let Some(listen) = cli.listen {
        config.listen = listen;
    }
    let addr = config.listen.clone();

    let state = Arc::new(AppState { config });
    let app = build_app(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("ai-api-bridge listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
```

> Note: `main.rs` references `canonical`, `router`, `sse`, `upstream`, `wire` modules created in later tasks. To compile Task 1 alone, temporarily create those files as empty stubs (`// placeholder`). They are filled in by their tasks. The stubs are removed implicitly as each task replaces them.

Create empty stub files now so it builds:
- `src/canonical.rs` → `// filled in Task 2`
- `src/router.rs` → `// filled in Task 8`
- `src/sse.rs` → `// filled in Task 5`
- `src/upstream.rs` → `// filled in Task 9`
- `src/wire/mod.rs` → `pub mod chat;\npub mod responses;\npub mod anthropic;`
- `src/wire/chat.rs`, `src/wire/responses.rs`, `src/wire/anthropic.rs` → `// filled in later`

- [ ] **Step 4: Run the config tests; expect FAIL→PASS**

Run: `cargo test config::tests`
Expected: compiles and PASSES (two tests). If it fails to compile because of missing later modules, ensure the empty stub files from Step 3 exist.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/
git commit -m "feat: scaffold ai-api-bridge — deps, config, health server"
```

---

## Task 2: Canonical model

**Files:**
- Create (replace stub): `src/canonical.rs`

- [ ] **Step 1: Write the failing test for the reasoning-effort helpers**

Put this at the bottom of the new `src/canonical.rs` (full file below in Step 2):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_effort_roundtrip() {
        assert_eq!(ReasoningEffort::from_str_opt("xhigh"), Some(ReasoningEffort::XHigh));
        assert_eq!(ReasoningEffort::XHigh.as_str(), "xhigh");
        assert_eq!(ReasoningEffort::from_str_opt("bogus"), None);
    }

    #[test]
    fn tool_choice_default_is_auto() {
        assert_eq!(ToolChoice::default(), ToolChoice::Auto);
    }
}
```

- [ ] **Step 2: Write `src/canonical.rs`**

```rust
//! Provider-neutral request + streaming-event model. Every wire format
//! translates to/from these types, so we never translate format→format directly.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub tool_choice: ToolChoice,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub parallel_tool_calls: Option<bool>,
    pub stream: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    User(String),
    Assistant { text: Option<String>, tool_calls: Vec<ToolCall> },
    Tool { call_id: String, output: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    /// Raw JSON arguments string (as the model emits them).
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Function(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::XHigh => "xhigh",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        Some(match s {
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::XHigh,
            _ => return None,
        })
    }
}

/// Provider-neutral streaming events. Both wire formats serialize to/from this.
#[derive(Debug, Clone, PartialEq)]
pub enum CanonicalEvent {
    Created { response_id: String, model: String },
    ReasoningDelta { text: String },
    TextDelta { text: String },
    ToolCallStart { index: u32, call_id: String, name: String },
    ToolCallArgsDelta { index: u32, delta: String },
    ToolCallDone { index: u32 },
    Usage { input_tokens: u32, output_tokens: u32, total_tokens: u32 },
    Completed,
    Error { message: String, status: u16 },
}

// (tests from Step 1 go here)
```

- [ ] **Step 3: Add `pub mod canonical;` is already in main.rs — run the tests**

Run: `cargo test canonical::tests`
Expected: 2 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/canonical.rs
git commit -m "feat: canonical request + event model"
```

---

## Task 3: Responses inbound — `parse_request`

**Files:**
- Modify (replace stub): `src/wire/responses.rs`

- [ ] **Step 1: Write failing tests**

Add to the bottom of `src/wire/responses.rs` (file body in Step 2):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::*;
    use serde_json::json;

    #[test]
    fn parses_instructions_and_string_input() {
        let body = json!({
            "model": "gpt-5.5",
            "instructions": "be terse",
            "input": "hello",
            "stream": true
        });
        let req = parse_request(&body).unwrap();
        assert_eq!(req.model, "gpt-5.5");
        assert_eq!(req.system.as_deref(), Some("be terse"));
        assert_eq!(req.messages, vec![Message::User("hello".into())]);
        assert!(req.stream);
    }

    #[test]
    fn parses_array_input_with_tool_roundtrip() {
        let body = json!({
            "model": "gpt-5.5",
            "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "weather?"}]},
                {"type": "function_call", "call_id": "c1", "name": "get_weather",
                 "arguments": "{\"city\":\"SF\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "sunny"}
            ],
            "tools": [{"type": "function", "name": "get_weather",
                       "description": "w", "parameters": {"type": "object"}}],
            "tool_choice": "auto"
        });
        let req = parse_request(&body).unwrap();
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[0], Message::User("weather?".into()));
        match &req.messages[1] {
            Message::Assistant { tool_calls, .. } => {
                assert_eq!(tool_calls[0].name, "get_weather");
                assert_eq!(tool_calls[0].call_id, "c1");
            }
            other => panic!("expected assistant tool call, got {other:?}"),
        }
        assert_eq!(req.messages[2], Message::Tool { call_id: "c1".into(), output: "sunny".into() });
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "get_weather");
        assert_eq!(req.tool_choice, ToolChoice::Auto);
    }

    #[test]
    fn parses_reasoning_effort_and_max_tokens() {
        let body = json!({
            "model": "gpt-5.5",
            "input": "hi",
            "reasoning": {"effort": "xhigh"},
            "max_output_tokens": 2048
        });
        let req = parse_request(&body).unwrap();
        assert_eq!(req.reasoning_effort, Some(ReasoningEffort::XHigh));
        assert_eq!(req.max_output_tokens, Some(2048));
    }

    #[test]
    fn rejects_missing_model() {
        let err = parse_request(&json!({"input": "x"})).unwrap_err();
        assert!(matches!(err, crate::error::BridgeError::BadRequest(_)));
    }
}
```

- [ ] **Step 2: Write the parser (top of `src/wire/responses.rs`)**

```rust
//! OpenAI Responses API wire format — inbound (server) side.

use serde_json::Value;

use crate::canonical::*;
use crate::error::BridgeError;

pub fn parse_request(body: &Value) -> Result<CanonicalRequest, BridgeError> {
    let obj = body
        .as_object()
        .ok_or_else(|| BridgeError::BadRequest("body must be a JSON object".into()))?;

    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BridgeError::BadRequest("missing `model`".into()))?
        .to_string();

    let mut system = obj
        .get("instructions")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut messages = Vec::new();
    match obj.get("input") {
        Some(Value::String(s)) => messages.push(Message::User(s.clone())),
        Some(Value::Array(items)) => {
            for item in items {
                parse_input_item(item, &mut messages, &mut system);
            }
        }
        Some(_) => return Err(BridgeError::BadRequest("`input` must be string or array".into())),
        None => {}
    }

    let tools = parse_tools(obj.get("tools"))?;
    let tool_choice = parse_tool_choice(obj.get("tool_choice"));
    let temperature = obj.get("temperature").and_then(|v| v.as_f64()).map(|f| f as f32);
    let top_p = obj.get("top_p").and_then(|v| v.as_f64()).map(|f| f as f32);
    let max_output_tokens = obj.get("max_output_tokens").and_then(|v| v.as_u64()).map(|n| n as u32);
    let reasoning_effort = obj
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(|e| e.as_str())
        .and_then(ReasoningEffort::from_str_opt);
    let parallel_tool_calls = obj.get("parallel_tool_calls").and_then(|v| v.as_bool());
    let stream = obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    Ok(CanonicalRequest {
        model,
        system,
        messages,
        tools,
        tool_choice,
        temperature,
        top_p,
        max_output_tokens,
        reasoning_effort,
        parallel_tool_calls,
        stream,
    })
}

fn parse_input_item(item: &Value, messages: &mut Vec<Message>, system: &mut Option<String>) {
    let kind = item.get("type").and_then(|v| v.as_str()).unwrap_or("message");
    match kind {
        "message" => {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let text = extract_text(item.get("content"));
            match role {
                "system" | "developer" => append_system(system, &text),
                "assistant" => messages.push(Message::Assistant { text: Some(text), tool_calls: vec![] }),
                _ => messages.push(Message::User(text)),
            }
        }
        "function_call" => {
            let call = ToolCall {
                call_id: str_field(item, "call_id"),
                name: str_field(item, "name"),
                arguments: str_field(item, "arguments"),
            };
            if let Some(Message::Assistant { tool_calls, .. }) = messages.last_mut() {
                tool_calls.push(call);
            } else {
                messages.push(Message::Assistant { text: None, tool_calls: vec![call] });
            }
        }
        "function_call_output" => {
            let output = match item.get("output") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            messages.push(Message::Tool { call_id: str_field(item, "call_id"), output });
        }
        // "reasoning" items are intentionally dropped (Chat Completions is stateless).
        _ => {}
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn extract_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                    out.push_str(t);
                }
            }
            out
        }
        _ => String::new(),
    }
}

fn append_system(system: &mut Option<String>, text: &str) {
    match system {
        Some(s) => {
            s.push('\n');
            s.push_str(text);
        }
        None => *system = Some(text.to_string()),
    }
}

fn parse_tools(v: Option<&Value>) -> Result<Vec<ToolDef>, BridgeError> {
    let Some(Value::Array(arr)) = v else { return Ok(vec![]) };
    let mut tools = Vec::new();
    for t in arr {
        if t.get("type").and_then(|x| x.as_str()) != Some("function") {
            continue;
        }
        let name = t
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or_else(|| BridgeError::BadRequest("tool missing `name`".into()))?
            .to_string();
        tools.push(ToolDef {
            name,
            description: t.get("description").and_then(|x| x.as_str()).map(|s| s.to_string()),
            parameters: t.get("parameters").cloned().unwrap_or(Value::Null),
        });
    }
    Ok(tools)
}

fn parse_tool_choice(v: Option<&Value>) -> ToolChoice {
    match v {
        Some(Value::String(s)) => match s.as_str() {
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Auto,
        },
        Some(Value::Object(o)) => o
            .get("name")
            .and_then(|n| n.as_str())
            .map(|n| ToolChoice::Function(n.to_string()))
            .unwrap_or(ToolChoice::Auto),
        _ => ToolChoice::Auto,
    }
}

// (tests from Step 1 go here)
```

- [ ] **Step 3: Run tests**

Run: `cargo test wire::responses::tests`
Expected: 4 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/wire/responses.rs
git commit -m "feat: Responses inbound request parsing"
```

---

## Task 4: Chat outbound — `build_request`

**Files:**
- Modify (replace stub): `src/wire/chat.rs`

- [ ] **Step 1: Write failing tests**

Add to bottom of `src/wire/chat.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::*;
    use crate::config::{Provider, WireName};
    use serde_json::json;
    use std::collections::HashMap;

    fn provider() -> Provider {
        Provider {
            wire: WireName::OpenaiChat,
            base_url: "https://opencode.ai/zen/v1".into(),
            api_key: Some("k".into()),
            model_prefix: Some("opencode/".into()),
            max_tokens_field: "max_tokens".into(),
            extra_headers: HashMap::new(),
        }
    }

    #[test]
    fn builds_messages_tools_and_stream_options() {
        let req = CanonicalRequest {
            model: "gpt-5.5".into(),
            system: Some("sys".into()),
            messages: vec![
                Message::User("hi".into()),
                Message::Assistant { text: None, tool_calls: vec![ToolCall {
                    call_id: "c1".into(), name: "f".into(), arguments: "{}".into() }] },
                Message::Tool { call_id: "c1".into(), output: "ok".into() },
            ],
            tools: vec![ToolDef { name: "f".into(), description: Some("d".into()), parameters: json!({"type":"object"}) }],
            tool_choice: ToolChoice::Required,
            temperature: Some(0.5),
            top_p: None,
            max_output_tokens: Some(100),
            reasoning_effort: Some(ReasoningEffort::High),
            parallel_tool_calls: Some(true),
            stream: true,
        };
        let body = build_request(&req, "opencode/gpt-5.5", &provider());
        assert_eq!(body["model"], "opencode/gpt-5.5");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][2]["tool_calls"][0]["function"]["name"], "f");
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["messages"][3]["tool_call_id"], "c1");
        assert_eq!(body["tools"][0]["function"]["name"], "f");
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn honors_max_tokens_field_name() {
        let mut p = provider();
        p.max_tokens_field = "max_completion_tokens".into();
        let req = CanonicalRequest {
            model: "m".into(), system: None, messages: vec![Message::User("x".into())],
            tools: vec![], tool_choice: ToolChoice::Auto, temperature: None, top_p: None,
            max_output_tokens: Some(42), reasoning_effort: None, parallel_tool_calls: None, stream: false,
        };
        let body = build_request(&req, "m", &p);
        assert_eq!(body["max_completion_tokens"], 42);
        assert!(body.get("stream").is_none());
    }
}
```

- [ ] **Step 2: Write `build_request` (top of `src/wire/chat.rs`)**

```rust
//! OpenAI Chat Completions wire format — outbound (client) side.

use serde_json::{json, Map, Value};

use crate::canonical::*;
use crate::config::Provider;

pub fn build_request(req: &CanonicalRequest, upstream_model: &str, provider: &Provider) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(sys) = &req.system {
        messages.push(json!({"role": "system", "content": sys}));
    }
    for m in &req.messages {
        match m {
            Message::User(text) => messages.push(json!({"role": "user", "content": text})),
            Message::Assistant { text, tool_calls } => {
                let mut obj = Map::new();
                obj.insert("role".into(), json!("assistant"));
                obj.insert("content".into(), text.clone().map(Value::String).unwrap_or(Value::Null));
                if !tool_calls.is_empty() {
                    let tcs: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| json!({
                            "id": tc.call_id,
                            "type": "function",
                            "function": {"name": tc.name, "arguments": tc.arguments}
                        }))
                        .collect();
                    obj.insert("tool_calls".into(), json!(tcs));
                }
                messages.push(Value::Object(obj));
            }
            Message::Tool { call_id, output } => {
                messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": output}))
            }
        }
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(upstream_model));
    body.insert("messages".into(), json!(messages));

    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({
                "type": "function",
                "function": {"name": t.name, "description": t.description, "parameters": t.parameters}
            }))
            .collect();
        body.insert("tools".into(), json!(tools));
        body.insert("tool_choice".into(), tool_choice_json(&req.tool_choice));
    }

    if let Some(t) = req.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        body.insert("top_p".into(), json!(p));
    }
    if let Some(m) = req.max_output_tokens {
        body.insert(provider.max_tokens_field.clone(), json!(m));
    }
    if let Some(e) = req.reasoning_effort {
        body.insert("reasoning_effort".into(), json!(e.as_str()));
    }
    if let Some(p) = req.parallel_tool_calls {
        body.insert("parallel_tool_calls".into(), json!(p));
    }
    if req.stream {
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }

    Value::Object(body)
}

fn tool_choice_json(tc: &ToolChoice) -> Value {
    match tc {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Function(name) => json!({"type": "function", "function": {"name": name}}),
    }
}

// (tests from Step 1 go here; ChatStreamParser + completion_to_events added in Task 6)
```

- [ ] **Step 3: Run tests**

Run: `cargo test wire::chat::tests`
Expected: 2 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/wire/chat.rs
git commit -m "feat: Chat Completions outbound request builder"
```

---

## Task 5: SSE decoder

**Files:**
- Modify (replace stub): `src/sse.rs`

- [ ] **Step 1: Write failing tests**

Add to bottom of `src/sse.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_across_chunks() {
        let mut d = SseDecoder::default();
        let mut items = d.push("data: {\"a\":1}\n\ndata: {\"b\"");
        items.extend(d.push(":2}\n\n"));
        assert_eq!(items, vec![
            SseItem::Data("{\"a\":1}".into()),
            SseItem::Data("{\"b\":2}".into()),
        ]);
    }

    #[test]
    fn recognizes_done_and_crlf() {
        let mut d = SseDecoder::default();
        let items = d.push("data: [DONE]\r\n\r\n");
        assert_eq!(items, vec![SseItem::Done]);
    }

    #[test]
    fn ignores_comment_and_event_lines() {
        let mut d = SseDecoder::default();
        let items = d.push(": ping\nevent: foo\ndata: {\"x\":true}\n\n");
        assert_eq!(items, vec![SseItem::Data("{\"x\":true}".into())]);
    }
}
```

- [ ] **Step 2: Write the decoder (top of `src/sse.rs`)**

```rust
//! Minimal Server-Sent Events decoder for upstream byte streams.

#[derive(Default)]
pub struct SseDecoder {
    buf: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SseItem {
    Data(String),
    Done,
}

impl SseDecoder {
    /// Feed a chunk of text; returns whatever complete events are now available.
    pub fn push(&mut self, chunk: &str) -> Vec<SseItem> {
        self.buf.push_str(&chunk.replace("\r\n", "\n"));
        let mut items = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let block: String = self.buf[..pos].to_string();
            self.buf.drain(..pos + 2);
            if let Some(item) = parse_event_block(&block) {
                items.push(item);
            }
        }
        items
    }
}

fn parse_event_block(block: &str) -> Option<SseItem> {
    let mut data = String::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest);
        }
        // event:, id:, and ":" comment lines are ignored
    }
    if data.is_empty() {
        return None;
    }
    if data.trim() == "[DONE]" {
        return Some(SseItem::Done);
    }
    Some(SseItem::Data(data))
}

// (tests from Step 1 go here)
```

- [ ] **Step 3: Run tests**

Run: `cargo test sse::tests`
Expected: 3 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/sse.rs
git commit -m "feat: SSE decoder"
```

---

## Task 6: Chat outbound — stream parser + non-stream parser

**Files:**
- Modify: `src/wire/chat.rs` (append parser code + tests)

- [ ] **Step 1: Write failing tests**

Append to the `tests` module already in `src/wire/chat.rs`:

```rust
    #[test]
    fn stream_parser_emits_text_then_usage() {
        let mut p = ChatStreamParser::new("resp_x".into());
        let mut evs = p.on_chunk(&json!({"model":"opencode/gpt-5.5",
            "choices":[{"delta":{"content":"Hel"}}]}));
        evs.extend(p.on_chunk(&json!({"choices":[{"delta":{"content":"lo"}}]})));
        evs.extend(p.on_chunk(&json!({"choices":[{"delta":{},"finish_reason":"stop"}]})));
        evs.extend(p.on_chunk(&json!({"choices":[],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}})));
        use CanonicalEvent::*;
        assert_eq!(evs[0], Created { response_id: "resp_x".into(), model: "opencode/gpt-5.5".into() });
        assert_eq!(evs[1], TextDelta { text: "Hel".into() });
        assert_eq!(evs[2], TextDelta { text: "lo".into() });
        assert_eq!(evs.last().unwrap(), &Usage { input_tokens: 3, output_tokens: 2, total_tokens: 5 });
    }

    #[test]
    fn stream_parser_handles_tool_call_fragments() {
        let mut p = ChatStreamParser::new("r".into());
        let mut evs = p.on_chunk(&json!({"model":"m","choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_1","function":{"name":"get","arguments":"{\"a\""}}]}}]}));
        evs.extend(p.on_chunk(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":":1}"}}]}}]})));
        evs.extend(p.on_chunk(&json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})));
        use CanonicalEvent::*;
        assert!(evs.contains(&ToolCallStart { index: 0, call_id: "call_1".into(), name: "get".into() }));
        assert!(evs.contains(&ToolCallArgsDelta { index: 0, delta: "{\"a\"".into() }));
        assert!(evs.contains(&ToolCallArgsDelta { index: 0, delta: ":1}".into() }));
        assert!(evs.contains(&ToolCallDone { index: 0 }));
    }

    #[test]
    fn completion_to_events_full_message() {
        let resp = json!({"model":"m","choices":[{"message":{"content":"hi",
            "tool_calls":[{"id":"c","function":{"name":"f","arguments":"{}"}}]}}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
        let evs = completion_to_events(&resp, "r1");
        use CanonicalEvent::*;
        assert_eq!(evs.first().unwrap(), &Created { response_id: "r1".into(), model: "m".into() });
        assert!(evs.contains(&TextDelta { text: "hi".into() }));
        assert!(evs.contains(&ToolCallStart { index: 0, call_id: "c".into(), name: "f".into() }));
        assert_eq!(evs.last().unwrap(), &Completed);
    }
```

- [ ] **Step 2: Append the parser implementation to `src/wire/chat.rs`** (after `tool_choice_json`, before the tests module)

```rust
use std::collections::HashSet;

/// Stateful translator: upstream Chat Completions stream chunks -> canonical events.
#[derive(Default)]
pub struct ChatStreamParser {
    response_id: String,
    model: String,
    created: bool,
    started_tools: HashSet<u32>,
}

impl ChatStreamParser {
    pub fn new(response_id: String) -> Self {
        Self { response_id, ..Default::default() }
    }

    pub fn on_chunk(&mut self, chunk: &Value) -> Vec<CanonicalEvent> {
        let mut events = Vec::new();
        if !self.created {
            self.model = chunk.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
            events.push(CanonicalEvent::Created {
                response_id: self.response_id.clone(),
                model: self.model.clone(),
            });
            self.created = true;
        }

        let choice = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first());

        if let Some(choice) = choice {
            if let Some(delta) = choice.get("delta") {
                if let Some(rc) = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(|v| v.as_str())
                {
                    if !rc.is_empty() {
                        events.push(CanonicalEvent::ReasoningDelta { text: rc.to_string() });
                    }
                }
                if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
                    if !c.is_empty() {
                        events.push(CanonicalEvent::TextDelta { text: c.to_string() });
                    }
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        if !self.started_tools.contains(&index) {
                            let call_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let name = tc.get("function").and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str()).unwrap_or("").to_string();
                            self.started_tools.insert(index);
                            events.push(CanonicalEvent::ToolCallStart { index, call_id, name });
                        }
                        if let Some(args) = tc.get("function").and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                        {
                            if !args.is_empty() {
                                events.push(CanonicalEvent::ToolCallArgsDelta { index, delta: args.to_string() });
                            }
                        }
                    }
                }
            }
            if choice.get("finish_reason").and_then(|v| v.as_str()).is_some() {
                let mut idxs: Vec<u32> = self.started_tools.iter().copied().collect();
                idxs.sort_unstable();
                for i in idxs {
                    events.push(CanonicalEvent::ToolCallDone { index: i });
                }
            }
        }

        if let Some(u) = chunk.get("usage") {
            if !u.is_null() {
                events.push(usage_event(u));
            }
        }
        events
    }
}

/// Translate a full (non-streaming) Chat Completions response into canonical events.
pub fn completion_to_events(resp: &Value, response_id: &str) -> Vec<CanonicalEvent> {
    let mut events = Vec::new();
    let model = resp.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    events.push(CanonicalEvent::Created { response_id: response_id.to_string(), model });

    if let Some(msg) = resp
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
    {
        if let Some(rc) = msg.get("reasoning_content").or_else(|| msg.get("reasoning")).and_then(|v| v.as_str()) {
            if !rc.is_empty() {
                events.push(CanonicalEvent::ReasoningDelta { text: rc.to_string() });
            }
        }
        if let Some(c) = msg.get("content").and_then(|v| v.as_str()) {
            if !c.is_empty() {
                events.push(CanonicalEvent::TextDelta { text: c.to_string() });
            }
        }
        if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for (i, tc) in tcs.iter().enumerate() {
                let index = i as u32;
                let call_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let name = tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let args = tc.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                events.push(CanonicalEvent::ToolCallStart { index, call_id, name });
                if !args.is_empty() {
                    events.push(CanonicalEvent::ToolCallArgsDelta { index, delta: args });
                }
                events.push(CanonicalEvent::ToolCallDone { index });
            }
        }
    }

    if let Some(u) = resp.get("usage") {
        if !u.is_null() {
            events.push(usage_event(u));
        }
    }
    events.push(CanonicalEvent::Completed);
    events
}

fn usage_event(u: &Value) -> CanonicalEvent {
    CanonicalEvent::Usage {
        input_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        output_tokens: u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test wire::chat::tests`
Expected: 5 tests PASS (2 from Task 4 + 3 new).

- [ ] **Step 4: Commit**

```bash
git add src/wire/chat.rs
git commit -m "feat: Chat Completions stream + non-stream response parsing"
```

---

## Task 7: Responses inbound — event emitter

**Files:**
- Modify: `src/wire/responses.rs` (append `ResponsesEmitter` + tests)

- [ ] **Step 1: Write failing tests**

Append to the `tests` module in `src/wire/responses.rs`:

```rust
    use crate::canonical::CanonicalEvent::*;

    fn event_names(frames: &[SseFrame]) -> Vec<String> {
        frames.iter().map(|f| f.event.clone()).collect()
    }

    #[test]
    fn emits_message_sequence_for_text() {
        let mut e = ResponsesEmitter::new();
        let mut frames = Vec::new();
        frames.extend(e.on_event(&Created { response_id: "r".into(), model: "m".into() }));
        frames.extend(e.on_event(&TextDelta { text: "Hi".into() }));
        frames.extend(e.on_event(&Usage { input_tokens: 1, output_tokens: 1, total_tokens: 2 }));
        frames.extend(e.on_event(&Completed));
        let names = event_names(&frames);
        assert_eq!(names.first().unwrap(), "response.created");
        assert!(names.contains(&"response.output_item.added".to_string()));
        assert!(names.contains(&"response.content_part.added".to_string()));
        assert!(names.contains(&"response.output_text.delta".to_string()));
        assert_eq!(names.last().unwrap(), "response.completed");
        // delta payload carries the text
        let delta = frames.iter().find(|f| f.event == "response.output_text.delta").unwrap();
        assert_eq!(delta.data["delta"], "Hi");
        // completed payload carries usage + the assembled message
        let completed = frames.last().unwrap();
        assert_eq!(completed.data["response"]["usage"]["total_tokens"], 2);
        assert_eq!(completed.data["response"]["output"][0]["content"][0]["text"], "Hi");
    }

    #[test]
    fn emits_function_call_sequence() {
        let mut e = ResponsesEmitter::new();
        let mut frames = Vec::new();
        frames.extend(e.on_event(&Created { response_id: "r".into(), model: "m".into() }));
        frames.extend(e.on_event(&ToolCallStart { index: 0, call_id: "c1".into(), name: "f".into() }));
        frames.extend(e.on_event(&ToolCallArgsDelta { index: 0, delta: "{}".into() }));
        frames.extend(e.on_event(&ToolCallDone { index: 0 }));
        frames.extend(e.on_event(&Completed));
        let names = event_names(&frames);
        assert!(names.contains(&"response.function_call_arguments.delta".to_string()));
        assert!(names.contains(&"response.function_call_arguments.done".to_string()));
        let done = frames.iter().find(|f| f.event == "response.function_call_arguments.done").unwrap();
        assert_eq!(done.data["arguments"], "{}");
        let completed = frames.last().unwrap();
        assert_eq!(completed.data["response"]["output"][0]["type"], "function_call");
        assert_eq!(completed.data["response"]["output"][0]["name"], "f");
    }

    #[test]
    fn final_response_available_after_completed() {
        let mut e = ResponsesEmitter::new();
        e.on_event(&Created { response_id: "r".into(), model: "m".into() });
        e.on_event(&TextDelta { text: "ok".into() });
        e.on_event(&Completed);
        let resp = e.final_response();
        assert_eq!(resp["status"], "completed");
        assert_eq!(resp["output"][0]["content"][0]["text"], "ok");
    }
```

- [ ] **Step 2: Append the emitter to `src/wire/responses.rs`** (before the tests module)

```rust
use serde_json::json;

/// One SSE frame: an event name + a JSON payload.
#[derive(Debug, Clone)]
pub struct SseFrame {
    pub event: String,
    pub data: Value,
}

fn frame(event: &str, data: Value) -> SseFrame {
    SseFrame { event: event.to_string(), data }
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

struct OpenItem {
    id: String,
    output_index: u32,
}

struct ToolItem {
    id: String,
    output_index: u32,
    call_id: String,
    name: String,
    args: String,
}

/// Stateful translator: canonical events -> Responses API SSE frames.
#[derive(Default)]
pub struct ResponsesEmitter {
    response_id: String,
    model: String,
    next_output_index: u32,
    reasoning_item: Option<OpenItem>,
    reasoning_text: String,
    message_item: Option<OpenItem>,
    message_text: String,
    tools: std::collections::BTreeMap<u32, ToolItem>,
    usage: Option<(u32, u32, u32)>,
    final_items: Vec<Value>,
}

impl ResponsesEmitter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_event(&mut self, ev: &CanonicalEvent) -> Vec<SseFrame> {
        let mut f = Vec::new();
        match ev {
            CanonicalEvent::Created { response_id, model } => {
                self.response_id = response_id.clone();
                self.model = model.clone();
                f.push(frame("response.created", json!({
                    "type": "response.created",
                    "response": self.skeleton("in_progress")
                })));
            }
            CanonicalEvent::ReasoningDelta { text } => {
                self.ensure_reasoning_open(&mut f);
                self.reasoning_text.push_str(text);
                let item = self.reasoning_item.as_ref().unwrap();
                f.push(frame("response.reasoning_summary_text.delta", json!({
                    "type": "response.reasoning_summary_text.delta",
                    "item_id": item.id, "output_index": item.output_index,
                    "summary_index": 0, "delta": text
                })));
            }
            CanonicalEvent::TextDelta { text } => {
                self.close_reasoning(&mut f);
                self.ensure_message_open(&mut f);
                self.message_text.push_str(text);
                let item = self.message_item.as_ref().unwrap();
                f.push(frame("response.output_text.delta", json!({
                    "type": "response.output_text.delta",
                    "item_id": item.id, "output_index": item.output_index,
                    "content_index": 0, "delta": text
                })));
            }
            CanonicalEvent::ToolCallStart { index, call_id, name } => {
                self.close_reasoning(&mut f);
                self.close_message(&mut f);
                let output_index = self.alloc_index();
                let id = new_id("fc");
                f.push(frame("response.output_item.added", json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": {"type": "function_call", "id": id, "call_id": call_id,
                             "name": name, "arguments": "", "status": "in_progress"}
                })));
                self.tools.insert(*index, ToolItem {
                    id, output_index, call_id: call_id.clone(), name: name.clone(), args: String::new(),
                });
            }
            CanonicalEvent::ToolCallArgsDelta { index, delta } => {
                if let Some(t) = self.tools.get_mut(index) {
                    t.args.push_str(delta);
                    f.push(frame("response.function_call_arguments.delta", json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": t.id, "output_index": t.output_index, "delta": delta
                    })));
                }
            }
            CanonicalEvent::ToolCallDone { index } => {
                if let Some(t) = self.tools.get(index) {
                    let item = json!({"type": "function_call", "id": t.id, "call_id": t.call_id,
                        "name": t.name, "arguments": t.args, "status": "completed"});
                    f.push(frame("response.function_call_arguments.done", json!({
                        "type": "response.function_call_arguments.done",
                        "item_id": t.id, "output_index": t.output_index, "arguments": t.args
                    })));
                    f.push(frame("response.output_item.done", json!({
                        "type": "response.output_item.done",
                        "output_index": t.output_index, "item": item.clone()
                    })));
                    self.final_items.push(item);
                }
            }
            CanonicalEvent::Usage { input_tokens, output_tokens, total_tokens } => {
                self.usage = Some((*input_tokens, *output_tokens, *total_tokens));
            }
            CanonicalEvent::Completed => {
                self.close_reasoning(&mut f);
                self.close_message(&mut f);
                f.push(frame("response.completed", json!({
                    "type": "response.completed",
                    "response": self.completed_response()
                })));
            }
            CanonicalEvent::Error { message, status } => {
                f.push(frame("response.failed", json!({
                    "type": "response.failed",
                    "response": {"id": self.response_id, "status": "failed",
                        "error": {"code": status, "message": message}}
                })));
            }
        }
        f
    }

    /// The full Responses object (valid after a `Completed` event has been processed).
    pub fn final_response(&self) -> Value {
        self.completed_response()
    }

    fn alloc_index(&mut self) -> u32 {
        let i = self.next_output_index;
        self.next_output_index += 1;
        i
    }

    fn ensure_reasoning_open(&mut self, f: &mut Vec<SseFrame>) {
        if self.reasoning_item.is_some() {
            return;
        }
        let output_index = self.alloc_index();
        let id = new_id("rs");
        f.push(frame("response.output_item.added", json!({
            "type": "response.output_item.added", "output_index": output_index,
            "item": {"type": "reasoning", "id": id, "summary": []}
        })));
        self.reasoning_item = Some(OpenItem { id, output_index });
    }

    fn close_reasoning(&mut self, f: &mut Vec<SseFrame>) {
        if let Some(item) = self.reasoning_item.take() {
            let item_json = json!({"type": "reasoning", "id": item.id,
                "summary": [{"type": "summary_text", "text": self.reasoning_text}]});
            f.push(frame("response.output_item.done", json!({
                "type": "response.output_item.done",
                "output_index": item.output_index, "item": item_json.clone()
            })));
            self.final_items.push(item_json);
        }
    }

    fn ensure_message_open(&mut self, f: &mut Vec<SseFrame>) {
        if self.message_item.is_some() {
            return;
        }
        let output_index = self.alloc_index();
        let id = new_id("msg");
        f.push(frame("response.output_item.added", json!({
            "type": "response.output_item.added", "output_index": output_index,
            "item": {"type": "message", "id": id, "role": "assistant",
                     "status": "in_progress", "content": []}
        })));
        f.push(frame("response.content_part.added", json!({
            "type": "response.content_part.added", "item_id": id,
            "output_index": output_index, "content_index": 0,
            "part": {"type": "output_text", "text": "", "annotations": []}
        })));
        self.message_item = Some(OpenItem { id, output_index });
    }

    fn close_message(&mut self, f: &mut Vec<SseFrame>) {
        if let Some(item) = self.message_item.take() {
            f.push(frame("response.output_text.done", json!({
                "type": "response.output_text.done", "item_id": item.id,
                "output_index": item.output_index, "content_index": 0, "text": self.message_text
            })));
            f.push(frame("response.content_part.done", json!({
                "type": "response.content_part.done", "item_id": item.id,
                "output_index": item.output_index, "content_index": 0,
                "part": {"type": "output_text", "text": self.message_text, "annotations": []}
            })));
            let item_json = json!({"type": "message", "id": item.id, "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": self.message_text, "annotations": []}]});
            f.push(frame("response.output_item.done", json!({
                "type": "response.output_item.done",
                "output_index": item.output_index, "item": item_json.clone()
            })));
            self.final_items.push(item_json);
        }
    }

    fn skeleton(&self, status: &str) -> Value {
        json!({"id": self.response_id, "object": "response", "status": status,
            "model": self.model, "output": []})
    }

    fn completed_response(&self) -> Value {
        let (i, o, t) = self.usage.unwrap_or((0, 0, 0));
        json!({"id": self.response_id, "object": "response", "status": "completed",
            "model": self.model, "output": self.final_items,
            "usage": {"input_tokens": i, "output_tokens": o, "total_tokens": t}})
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test wire::responses::tests`
Expected: 7 tests PASS (4 from Task 3 + 3 new).

- [ ] **Step 4: Commit**

```bash
git add src/wire/responses.rs
git commit -m "feat: Responses inbound event emitter (streaming + final object)"
```

---

## Task 8: Router

**Files:**
- Modify (replace stub): `src/router.rs`

- [ ] **Step 1: Write failing tests**

```rust
//! Model alias -> (provider, upstream model) resolution.

use crate::config::{Config, Provider};
use crate::error::BridgeError;

pub struct Resolved<'a> {
    pub provider_name: String,
    pub provider: &'a Provider,
    pub upstream_model: String,
}

pub fn resolve<'a>(cfg: &'a Config, alias: &str) -> Result<Resolved<'a>, BridgeError> {
    if let Some(route) = cfg.routes.iter().find(|r| r.alias == alias) {
        let provider = cfg.providers.get(&route.provider).ok_or_else(|| {
            BridgeError::Internal(format!("route references unknown provider {}", route.provider))
        })?;
        return Ok(Resolved {
            provider_name: route.provider.clone(),
            provider,
            upstream_model: route.model.clone(),
        });
    }

    let provider_name = cfg
        .default_provider
        .clone()
        .ok_or_else(|| BridgeError::UnknownModel(alias.to_string()))?;
    let provider = cfg.providers.get(&provider_name).ok_or_else(|| {
        BridgeError::Internal(format!("default provider {provider_name} not configured"))
    })?;
    let upstream_model = match &provider.model_prefix {
        Some(prefix) if !alias.contains('/') => format!("{prefix}{alias}"),
        _ => alias.to_string(),
    };
    Ok(Resolved { provider_name, provider, upstream_model })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg() -> Config {
        Config::from_toml(
            r#"
default_provider = "zen"
[providers.zen]
wire = "openai-chat"
base_url = "https://opencode.ai/zen/v1"
model_prefix = "opencode/"
[[routes]]
alias = "fast"
provider = "zen"
model = "opencode/gpt-5.5-mini"
"#,
        )
        .unwrap()
    }

    #[test]
    fn explicit_route_wins() {
        let c = cfg();
        let r = resolve(&c, "fast").unwrap();
        assert_eq!(r.upstream_model, "opencode/gpt-5.5-mini");
    }

    #[test]
    fn default_provider_applies_prefix() {
        let c = cfg();
        let r = resolve(&c, "gpt-5.5").unwrap();
        assert_eq!(r.provider_name, "zen");
        assert_eq!(r.upstream_model, "opencode/gpt-5.5");
    }

    #[test]
    fn prefix_skipped_when_alias_already_qualified() {
        let c = cfg();
        let r = resolve(&c, "anthropic/claude").unwrap();
        assert_eq!(r.upstream_model, "anthropic/claude");
    }

    #[test]
    fn unknown_model_without_default_errors() {
        let c = Config::from_toml("[providers.x]\nwire=\"openai-chat\"\nbase_url=\"u\"").unwrap();
        let err = resolve(&c, "whatever").unwrap_err();
        assert!(matches!(err, BridgeError::UnknownModel(_)));
    }
}
```

- [ ] **Step 2: (code already written in Step 1 above)** — the file is complete.

- [ ] **Step 3: Run tests**

Run: `cargo test router::tests`
Expected: 4 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/router.rs
git commit -m "feat: model router (explicit routes + default-provider prefix)"
```

---

## Task 9: Upstream HTTP client

**Files:**
- Modify (replace stub): `src/upstream.rs`

- [ ] **Step 1: Write the client (no unit test — exercised end-to-end in Task 10)**

```rust
//! reqwest-based upstream client.

use bytes::Bytes;
use futures_util::Stream;
use serde_json::Value;

use crate::config::Provider;
use crate::error::BridgeError;

#[derive(Clone)]
pub struct Upstream {
    client: reqwest::Client,
}

impl Upstream {
    pub fn new() -> Self {
        Self { client: reqwest::Client::new() }
    }

    fn request(&self, provider: &Provider, path: &str, body: &Value) -> reqwest::RequestBuilder {
        let url = format!("{}{}", provider.base_url.trim_end_matches('/'), path);
        let mut rb = self.client.post(&url).json(body);
        if let Some(key) = &provider.api_key {
            rb = rb.bearer_auth(key);
        }
        for (k, v) in &provider.extra_headers {
            rb = rb.header(k, v);
        }
        rb
    }

    /// POST and return the response body as a byte stream (for SSE).
    pub async fn post_stream(
        &self,
        provider: &Provider,
        path: &str,
        body: &Value,
    ) -> Result<impl Stream<Item = reqwest::Result<Bytes>>, BridgeError> {
        let resp = self
            .request(provider, path, body)
            .send()
            .await
            .map_err(|e| BridgeError::UpstreamUnreachable(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(BridgeError::Upstream { status: status.as_u16(), message: truncate(&text, 500) });
        }
        Ok(resp.bytes_stream())
    }

    /// POST and return the parsed JSON body (non-streaming).
    pub async fn post_json(
        &self,
        provider: &Provider,
        path: &str,
        body: &Value,
    ) -> Result<Value, BridgeError> {
        let resp = self
            .request(provider, path, body)
            .send()
            .await
            .map_err(|e| BridgeError::UpstreamUnreachable(e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(BridgeError::Upstream { status: status.as_u16(), message: truncate(&text, 500) });
        }
        serde_json::from_str(&text).map_err(|e| BridgeError::Internal(format!("bad upstream JSON: {e}")))
    }
}

impl Default for Upstream {
    fn default() -> Self {
        Self::new()
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build`
Expected: builds clean.

- [ ] **Step 3: Commit**

```bash
git add src/upstream.rs
git commit -m "feat: upstream client (streaming + non-streaming POST)"
```

---

## Task 10: Wire it together — `/v1/responses` + end-to-end test

**Files:**
- Modify: `src/server.rs` (full handler + state + streaming pipeline)
- Create: `tests/pipeline.rs`

- [ ] **Step 1: Replace `src/server.rs` with the full server**

```rust
//! axum app + handlers + the streaming translation pipeline.

use std::convert::Infallible;
use std::sync::Arc;

use async_stream::stream;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt};
use serde_json::Value;

use crate::canonical::CanonicalEvent;
use crate::config::Config;
use crate::error::BridgeError;
use crate::router;
use crate::sse::{SseDecoder, SseItem};
use crate::upstream::Upstream;
use crate::wire::{chat, responses};

pub struct AppState {
    pub config: Config,
    pub upstream: Upstream,
}

pub fn build_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/responses", post(responses_handler))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), BridgeError> {
    let Some(expected) = &state.config.auth_token else {
        return Ok(());
    };
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    if got == Some(expected.as_str()) {
        Ok(())
    } else {
        Err(BridgeError::BadRequest("invalid bridge auth token".into()))
    }
}

async fn responses_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, BridgeError> {
    check_auth(&state, &headers)?;

    let req = responses::parse_request(&body)?;
    let resolved = router::resolve(&state.config, &req.model)?;
    let response_id = format!("resp_{}", uuid::Uuid::new_v4().simple());

    tracing::info!(model = %req.model, provider = %resolved.provider_name,
        upstream_model = %resolved.upstream_model, stream = req.stream, "responses request");

    let chat_body = chat::build_request(&req, &resolved.upstream_model, resolved.provider);

    if req.stream {
        let byte_stream = state
            .upstream
            .post_stream(resolved.provider, "/chat/completions", &chat_body)
            .await?;
        Ok(stream_sse(byte_stream, response_id).into_response())
    } else {
        let upstream_resp = state
            .upstream
            .post_json(resolved.provider, "/chat/completions", &chat_body)
            .await?;
        let events = chat::completion_to_events(&upstream_resp, &response_id);
        let mut emitter = responses::ResponsesEmitter::new();
        for ev in &events {
            emitter.on_event(ev);
        }
        Ok(Json(emitter.final_response()).into_response())
    }
}

fn stream_sse(
    byte_stream: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    response_id: String,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let s = stream! {
        let mut decoder = SseDecoder::default();
        let mut parser = chat::ChatStreamParser::new(response_id);
        let mut emitter = responses::ResponsesEmitter::new();
        let mut completed = false;

        futures_util::pin_mut!(byte_stream);
        while let Some(chunk) = byte_stream.next().await {
            match chunk {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    for item in decoder.push(&text) {
                        match item {
                            SseItem::Data(d) => {
                                let Ok(json) = serde_json::from_str::<Value>(&d) else { continue };
                                for cev in parser.on_chunk(&json) {
                                    for fr in emitter.on_event(&cev) {
                                        yield Ok(Event::default().event(fr.event).data(fr.data.to_string()));
                                    }
                                }
                            }
                            SseItem::Done => {
                                for fr in emitter.on_event(&CanonicalEvent::Completed) {
                                    yield Ok(Event::default().event(fr.event).data(fr.data.to_string()));
                                }
                                completed = true;
                            }
                        }
                    }
                }
                Err(e) => {
                    let ev = CanonicalEvent::Error { message: e.to_string(), status: 502 };
                    for fr in emitter.on_event(&ev) {
                        yield Ok(Event::default().event(fr.event).data(fr.data.to_string()));
                    }
                    completed = true;
                    break;
                }
            }
        }

        if !completed {
            for fr in emitter.on_event(&CanonicalEvent::Completed) {
                yield Ok(Event::default().event(fr.event).data(fr.data.to_string()));
            }
        }
    };
    Sse::new(s)
}
```

- [ ] **Step 2: Update `main.rs` to put `Upstream` in state**

In `src/main.rs`, change the state construction line:

```rust
    let state = Arc::new(AppState { config, upstream: crate::upstream::Upstream::new() });
```

- [ ] **Step 3: Write the end-to-end test against a mock upstream**

Create `tests/pipeline.rs`:

```rust
//! End-to-end: Codex-shaped Responses request -> bridge -> mock Zen upstream -> Responses SSE.

use std::sync::Arc;

use axum::routing::post;
use axum::Router;
use serde_json::json;

use ai_api_bridge::config::Config;
use ai_api_bridge::server::{build_app, AppState};
use ai_api_bridge::upstream::Upstream;

// A mock upstream that replays a fixed Chat Completions SSE stream.
async fn mock_chat() -> axum::response::Response {
    let body = concat!(
        "data: {\"model\":\"opencode/gpt-5.5\",\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
        "data: [DONE]\n\n",
    );
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(body))
        .unwrap()
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn responses_streaming_end_to_end() {
    // 1. mock upstream
    let upstream_url = spawn(Router::new().route("/chat/completions", post(mock_chat))).await;

    // 2. bridge configured to use the mock as the "zen" provider
    let cfg = Config::from_toml(&format!(
        r#"
default_provider = "zen"
[providers.zen]
wire = "openai-chat"
base_url = "{upstream_url}"
model_prefix = "opencode/"
"#
    ))
    .unwrap();
    let bridge_url = spawn(build_app(Arc::new(AppState { config: cfg, upstream: Upstream::new() }))).await;

    // 3. send a Codex-shaped Responses request
    let resp = reqwest::Client::new()
        .post(format!("{bridge_url}/v1/responses"))
        .json(&json!({"model": "gpt-5.5", "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let text = resp.text().await.unwrap();

    // 4. assert the translated Responses SSE
    assert!(text.contains("event: response.created"));
    assert!(text.contains("event: response.output_text.delta"));
    assert!(text.contains("Hello"));
    assert!(text.contains("world"));
    assert!(text.contains("event: response.completed"));
    assert!(text.contains("\"total_tokens\":7"));
}

#[tokio::test]
async fn unknown_model_returns_400() {
    let cfg = Config::from_toml("[providers.zen]\nwire=\"openai-chat\"\nbase_url=\"http://127.0.0.1:1\"").unwrap();
    let bridge_url = spawn(build_app(Arc::new(AppState { config: cfg, upstream: Upstream::new() }))).await;
    let resp = reqwest::Client::new()
        .post(format!("{bridge_url}/v1/responses"))
        .json(&json!({"model": "nope", "input": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}
```

- [ ] **Step 4: Make the crate testable as a library**

Integration tests in `tests/` need the modules public. Create `src/lib.rs`:

```rust
pub mod canonical;
pub mod config;
pub mod error;
pub mod router;
pub mod server;
pub mod sse;
pub mod upstream;
pub mod wire;
```

Then change `src/main.rs` to use the library crate instead of declaring modules. Replace the top `mod ...;` lines with:

```rust
use ai_api_bridge::config::Config;
use ai_api_bridge::server::{build_app, AppState};
use ai_api_bridge::upstream::Upstream;
```

and remove the per-module `mod` declarations (they now live in `lib.rs`). Update the state line to use `Upstream::new()` as in Step 2. Add `reqwest` to `[dev-dependencies]` so the test can use it:

```toml
[dev-dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "time"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
```

- [ ] **Step 5: Run the full suite**

Run: `cargo test`
Expected: all unit tests + `responses_streaming_end_to_end` + `unknown_model_returns_400` PASS.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/ tests/
git commit -m "feat: /v1/responses handler + streaming pipeline + e2e test"
```

---

## Task 11: Robustness — `/v1/models`, chat-inbound follow-on, anthropic stub

**Files:**
- Modify: `src/server.rs`, `src/wire/responses.rs` (add a Chat-inbound emitter reuse), `src/wire/anthropic.rs`

- [ ] **Step 1: Add `/v1/models` test + route**

In `tests/pipeline.rs` add:

```rust
#[tokio::test]
async fn lists_models() {
    let cfg = Config::from_toml(
        "default_provider=\"zen\"\n[providers.zen]\nwire=\"openai-chat\"\nbase_url=\"u\"\n[[routes]]\nalias=\"fast\"\nprovider=\"zen\"\nmodel=\"opencode/x\"",
    ).unwrap();
    let url = spawn(build_app(Arc::new(AppState { config: cfg, upstream: Upstream::new() }))).await;
    let body: serde_json::Value = reqwest::get(format!("{url}/v1/models")).await.unwrap().json().await.unwrap();
    assert_eq!(body["object"], "list");
    assert!(body["data"].as_array().unwrap().iter().any(|m| m["id"] == "fast"));
}
```

In `src/server.rs` add the route `.route("/v1/models", get(list_models))` and:

```rust
async fn list_models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let data: Vec<Value> = state
        .config
        .routes
        .iter()
        .map(|r| json!({"id": r.alias, "object": "model", "owned_by": r.provider}))
        .collect();
    Json(json!({"object": "list", "data": data}))
}
```

Add `use serde_json::json;` if not present.

- [ ] **Step 2: Add `/v1/chat/completions` inbound (reuses canonical IR)**

The simplest correct v1 behavior: accept Chat Completions inbound, parse to canonical, run the same outbound + pipeline, but serialize the response **back as Chat Completions**. To stay YAGNI and avoid a second emitter now, implement chat-inbound as: parse → canonical → outbound chat → **pass the upstream stream through unchanged** (since inbound and outbound are both Chat Completions, no response translation is needed). Add to `src/wire/chat.rs`:

```rust
/// Parse a Chat Completions *request* body into the canonical request.
pub fn parse_request(body: &Value) -> Result<CanonicalRequest, BridgeError> {
    let obj = body.as_object().ok_or_else(|| BridgeError::BadRequest("body must be an object".into()))?;
    let model = obj.get("model").and_then(|v| v.as_str())
        .ok_or_else(|| BridgeError::BadRequest("missing `model`".into()))?.to_string();
    let mut system = None;
    let mut messages = Vec::new();
    if let Some(arr) = obj.get("messages").and_then(|v| v.as_array()) {
        for m in arr {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
            match role {
                "system" | "developer" => system = Some(content),
                "tool" => messages.push(Message::Tool {
                    call_id: m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    output: content,
                }),
                "assistant" => messages.push(Message::Assistant { text: Some(content), tool_calls: vec![] }),
                _ => messages.push(Message::User(content)),
            }
        }
    }
    let stream = obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(CanonicalRequest {
        model, system, messages, tools: vec![], tool_choice: ToolChoice::Auto,
        temperature: obj.get("temperature").and_then(|v| v.as_f64()).map(|f| f as f32),
        top_p: None, max_output_tokens: obj.get("max_tokens").and_then(|v| v.as_u64()).map(|n| n as u32),
        reasoning_effort: None, parallel_tool_calls: None, stream,
    })
}
```

Add a `use crate::error::BridgeError;` import at the top of `chat.rs` if missing.

In `src/server.rs` add `.route("/v1/chat/completions", post(chat_handler))` and a handler that parses with `chat::parse_request`, resolves, builds the outbound chat body, and **streams the upstream bytes straight through** as `text/event-stream` (no Responses translation), or returns the upstream JSON for non-stream:

```rust
async fn chat_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, BridgeError> {
    check_auth(&state, &headers)?;
    let req = chat::parse_request(&body)?;
    let resolved = router::resolve(&state.config, &req.model)?;
    let chat_body = chat::build_request(&req, &resolved.upstream_model, resolved.provider);
    if req.stream {
        let byte_stream = state.upstream.post_stream(resolved.provider, "/chat/completions", &chat_body).await?;
        let s = stream! {
            futures_util::pin_mut!(byte_stream);
            while let Some(chunk) = byte_stream.next().await {
                match chunk {
                    Ok(b) => yield Ok::<_, Infallible>(Event::default().data(String::from_utf8_lossy(&b).to_string())),
                    Err(_) => break,
                }
            }
        };
        Ok(Sse::new(s).into_response())
    } else {
        let resp = state.upstream.post_json(resolved.provider, "/chat/completions", &chat_body).await?;
        Ok(Json(resp).into_response())
    }
}
```

> Note: passthrough streaming re-frames each upstream chunk as one SSE `data:` line, which is acceptable for Chat Completions clients. A byte-exact passthrough is a later refinement; YAGNI for v1.

- [ ] **Step 3: Anthropic stub returns 501**

Replace `src/wire/anthropic.rs`:

```rust
//! Anthropic Messages wire format — not implemented in v1.

use crate::error::BridgeError;

pub fn not_implemented() -> BridgeError {
    BridgeError::NotImplemented("anthropic-messages wire format".into())
}
```

Add a `/v1/messages` route returning that error:

```rust
async fn messages_handler() -> Result<Response, BridgeError> {
    Err(crate::wire::anthropic::not_implemented())
}
```
and `.route("/v1/messages", post(messages_handler))`.

- [ ] **Step 4: Run the suite**

Run: `cargo test`
Expected: all PASS including `lists_models`.

- [ ] **Step 5: Commit**

```bash
git add src/ tests/
git commit -m "feat: /v1/models, chat-completions inbound passthrough, anthropic 501 stub"
```

---

## Task 12: Docs + manual smoke

**Files:**
- Create: `README.md` (replace), `bridge.example.toml`, `CLAUDE.md`

- [ ] **Step 1: Write `bridge.example.toml`**

```toml
# Copy to bridge.toml and fill in your OpenCode Zen key (or set it via
# the BRIDGE_PROVIDERS_ZEN_API_KEY environment variable).
listen = "127.0.0.1:8282"
default_provider = "zen"
# auth_token = "set-me-to-require-a-bearer-token-from-clients"

[providers.zen]
wire = "openai-chat"
base_url = "https://opencode.ai/zen/v1"
model_prefix = "opencode/"   # gpt-5.5 -> opencode/gpt-5.5
# api_key = "..."            # prefer the env var above

# Optional explicit alias overrides:
# [[routes]]
# alias = "gpt-5.5"
# provider = "zen"
# model = "opencode/gpt-5.5"
```

- [ ] **Step 2: Write `README.md`**

````markdown
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
- `GET /health`.

## Config
See `bridge.example.toml`. The Zen key may be set inline (`api_key`) or via
`BRIDGE_PROVIDERS_<NAME>_API_KEY`.

## Architecture
See `docs/superpowers/specs/2026-06-05-ai-api-bridge-design.md`.
````

- [ ] **Step 3: Write `CLAUDE.md`** (the original `/init` deliverable)

```markdown
# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is
A local format-translating AI API proxy. Clients speak the OpenAI **Responses API**
(`/v1/responses`); the bridge translates to OpenAI **Chat Completions**
(`/chat/completions`) against a configured upstream (OpenCode Zen) and translates the
streaming response back. The point: use an OpenCode Zen account inside the Codex CLI.

## Commands
- Build: `cargo build` (release: `cargo build --release`)
- Run: `cargo run -- --config bridge.toml` (override port with `--listen host:port`)
- Test all: `cargo test`
- One test: `cargo test wire::responses::tests::emits_message_sequence_for_text`
- One module: `cargo test wire::chat::tests`
- Logs: `RUST_LOG=ai_api_bridge=debug cargo run -- --config bridge.toml`

## Architecture (the part that needs multiple files to understand)
Translation goes through a provider-neutral middle layer, never format→format directly:

  inbound wire format → `CanonicalRequest` → `router::resolve` → outbound wire format
  → upstream HTTP → upstream stream → `CanonicalEvent` stream → inbound wire format → client

- `canonical.rs` is the contract (`CanonicalRequest`, `CanonicalEvent`). Change it and
  every wire module is affected.
- `wire/responses.rs`: Responses **inbound** — `parse_request` (client req → canonical)
  and `ResponsesEmitter` (canonical events → Responses SSE frames, and `final_response()`
  for non-streaming).
- `wire/chat.rs`: Chat **outbound** — `build_request` (canonical → CC req),
  `ChatStreamParser` (CC SSE chunks → canonical events), `completion_to_events`
  (non-stream CC response → canonical events). Also `parse_request` for CC **inbound**.
- `server.rs`: `stream_sse` is the streaming pipeline that chains SseDecoder →
  ChatStreamParser → ResponsesEmitter. Each upstream chunk can yield several Responses
  SSE frames.
- `router.rs`: explicit `[[routes]]` win; otherwise the default provider's `model_prefix`
  is applied (`gpt-5.5` → `opencode/gpt-5.5`).

## Conventions / gotchas
- Wire-format dispatch is `enum` + `match`, not async trait objects — keep it that way;
  it keeps the streaming code simple.
- Emitter item IDs use random UUIDs, so assert on event **names** and payload fields in
  tests, not on IDs.
- `wire_api = "responses"` is the only protocol Codex supports; the bridge must speak it.
- Adding a wire format = fill the corresponding `parse_request`/`emit`/`build`/`parse`
  function and a route; the canonical layer is unchanged.
- Known limitation: no cross-turn encrypted-reasoning reuse (Chat Completions is
  stateless); inbound `reasoning` items are dropped. See the spec §11.

## Spec & plan
- Spec: `docs/superpowers/specs/2026-06-05-ai-api-bridge-design.md`
- Plan: `docs/superpowers/plans/2026-06-05-ai-api-bridge.md`
```

- [ ] **Step 4: Manual smoke test (record result, do not skip)**

```bash
cp bridge.example.toml bridge.toml
export BRIDGE_PROVIDERS_ZEN_API_KEY="<real Zen key>"
cargo run --release -- --config bridge.toml &
# point Codex at http://127.0.0.1:8282/v1 (config above), then:
BRIDGE_KEY=x codex exec "say hello in one word"
```
Expected: Codex streams a normal reply. If the upstream rejects the model name, adjust
`model_prefix` / add a `[[routes]]` entry. If it rejects `reasoning_effort` or `max_tokens`,
flip `max_tokens_field` or remove reasoning in config and note the upstream's quirk.

- [ ] **Step 5: Commit**

```bash
git add README.md bridge.example.toml CLAUDE.md
git commit -m "docs: README, example config, CLAUDE.md"
```

---

## Self-Review (completed by plan author)

**Spec coverage:**
- §3 canonical IR architecture → Tasks 2, 6, 7, 10 ✓
- §5.1 Responses inbound parse + emit → Tasks 3, 7 ✓
- §5.2 Chat outbound build + parse → Tasks 4, 6 ✓
- §6 router → Task 8 ✓
- §7 config (toml + env override) → Task 1 ✓
- §8 endpoints (`/v1/responses`, `/v1/chat/completions`, `/v1/messages`, `/v1/models`, `/health`) → Tasks 1, 10, 11 ✓
- §9 upstream client + SSE decoder → Tasks 5, 9 ✓
- §10 error mapping → Task 1 (error.rs) + exercised Task 10 ✓
- §11 reasoning-drop limitation → encoded in `responses::parse_request` (Task 3) + CLAUDE.md ✓
- §12 testing (golden + mock-upstream e2e) → Tasks 3–7 (golden) + Task 10 (e2e) ✓
- §16 Codex integration → README + CLAUDE.md (Task 12) ✓

**Placeholder scan:** the only "stub" files are the deliberate empty modules in Task 1 that each later task replaces; the anthropic 501 stub is intentional per spec non-goals. No TBD/TODO in implementation steps.

**Type consistency:** `parse_request`, `build_request`, `ChatStreamParser::{new,on_chunk}`, `completion_to_events`, `ResponsesEmitter::{new,on_event,final_response}`, `SseDecoder::push`/`SseItem`, `Upstream::{post_stream,post_json}`, `router::resolve`/`Resolved` are referenced consistently across Tasks 3–11. `usage_event` is private to `chat.rs` and used by both stream and non-stream parsers (DRY).
