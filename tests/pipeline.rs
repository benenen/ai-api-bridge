//! End-to-end: Codex-shaped Responses request -> bridge -> mock Zen upstream -> Responses SSE.

use std::sync::{Arc, RwLock};

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use serde_json::json;

use ai_api_bridge::config::Config;
use ai_api_bridge::server::{AppState, build_app};
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

async fn mock_chat_error() -> axum::response::Response {
    let body = concat!(
        "data: {\"error\":{\"message\":\"rate limited\",\"code\":429}}\n\n",
        "data: [DONE]\n\n",
    );
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(body))
        .unwrap()
}

// A non-streaming Chat Completions JSON response.
async fn mock_chat_json() -> axum::response::Response {
    let body = r#"{"model":"m","choices":[{"message":{"content":"Hello world"}}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#;
    axum::response::Response::builder()
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body))
        .unwrap()
}

// A non-streaming response carrying a real `cost` (for the usage-toggle test).
async fn mock_chat_json_cost() -> axum::response::Response {
    let body = r#"{"model":"m","choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":5,"completion_tokens":2},"cost":"0.5"}"#;
    axum::response::Response::builder()
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body))
        .unwrap()
}

// A streaming Chat Completions response that makes a tool call.
async fn mock_chat_tool() -> axum::response::Response {
    let body = concat!(
        "data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"total_tokens\":8}}\n\n",
        "data: [DONE]\n\n",
    );
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(body))
        .unwrap()
}

// Upstreams that fail with a fixed status (for reactive failover tests).
async fn mock_503() -> axum::response::Response {
    (StatusCode::SERVICE_UNAVAILABLE, "down").into_response()
}
async fn mock_400() -> axum::response::Response {
    (StatusCode::BAD_REQUEST, "bad request").into_response()
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn responses_streaming_end_to_end() {
    let upstream_url = spawn(Router::new().route("/chat/completions", post(mock_chat))).await;

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
    let bridge_url = spawn(build_app(Arc::new(AppState {
        config: RwLock::new(Arc::new(cfg)),
        upstream: Upstream::new(),
        status: Default::default(),
        pool: None,
        watchers: Default::default(),
        usage: std::sync::Arc::new(ai_api_bridge::usage::UsageMeter::new(None)),
        usage_on: Default::default(),
    })))
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{bridge_url}/v1/responses"))
        .json(&json!({"model": "gpt-5.5", "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let text = resp.text().await.unwrap();

    assert!(text.contains("event: response.created"));
    assert!(text.contains("event: response.output_text.delta"));
    assert!(text.contains("Hello"));
    assert!(text.contains("world"));
    assert!(text.contains("event: response.completed"));
    assert!(text.contains("\"total_tokens\":7"));
}

#[tokio::test]
async fn inband_upstream_error_becomes_response_failed() {
    let upstream_url = spawn(Router::new().route("/chat/completions", post(mock_chat_error))).await;
    let cfg = Config::from_toml(&format!(
        "default_provider=\"zen\"\n[providers.zen]\nwire=\"openai-chat\"\nbase_url=\"{upstream_url}\"\nmodel_prefix=\"opencode/\""
    ))
    .unwrap();
    let bridge_url = spawn(build_app(Arc::new(AppState {
        config: RwLock::new(Arc::new(cfg)),
        upstream: Upstream::new(),
        status: Default::default(),
        pool: None,
        watchers: Default::default(),
        usage: std::sync::Arc::new(ai_api_bridge::usage::UsageMeter::new(None)),
        usage_on: Default::default(),
    })))
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{bridge_url}/v1/responses"))
        .json(&json!({"model": "gpt-5.5", "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("event: response.failed"),
        "expected response.failed, got: {text}"
    );
    assert!(text.contains("rate limited"));
    assert!(!text.contains("event: response.completed"));
}

#[tokio::test]
async fn unknown_model_returns_400() {
    let cfg =
        Config::from_toml("[providers.zen]\nwire=\"openai-chat\"\nbase_url=\"http://127.0.0.1:1\"")
            .unwrap();
    let bridge_url = spawn(build_app(Arc::new(AppState {
        config: RwLock::new(Arc::new(cfg)),
        upstream: Upstream::new(),
        status: Default::default(),
        pool: None,
        watchers: Default::default(),
        usage: std::sync::Arc::new(ai_api_bridge::usage::UsageMeter::new(None)),
        usage_on: Default::default(),
    })))
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{bridge_url}/v1/responses"))
        .json(&json!({"model": "nope", "input": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn lists_models() {
    let cfg = Config::from_toml(
        "default_provider=\"zen\"\n[providers.zen]\nwire=\"openai-chat\"\nbase_url=\"u\"\n[[routes]]\nalias=\"fast\"\nprovider=\"zen\"\nmodel=\"opencode/x\"",
    )
    .unwrap();
    let url = spawn(build_app(Arc::new(AppState {
        config: RwLock::new(Arc::new(cfg)),
        upstream: Upstream::new(),
        status: Default::default(),
        pool: None,
        watchers: Default::default(),
        usage: std::sync::Arc::new(ai_api_bridge::usage::UsageMeter::new(None)),
        usage_on: Default::default(),
    })))
    .await;
    let body: serde_json::Value = reqwest::get(format!("{url}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["object"], "list");
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "fast")
    );
}

fn messages_bridge_url_for(upstream_url: &str) -> Config {
    Config::from_toml(&format!(
        "default_provider=\"zen\"\n[providers.zen]\nwire=\"openai-chat\"\nbase_url=\"{upstream_url}\""
    ))
    .unwrap()
}

#[tokio::test]
async fn messages_streaming_end_to_end() {
    let upstream_url = spawn(Router::new().route("/chat/completions", post(mock_chat))).await;
    let cfg = messages_bridge_url_for(&upstream_url);
    let bridge_url = spawn(build_app(Arc::new(AppState {
        config: RwLock::new(Arc::new(cfg)),
        upstream: Upstream::new(),
        status: Default::default(),
        pool: None,
        watchers: Default::default(),
        usage: std::sync::Arc::new(ai_api_bridge::usage::UsageMeter::new(None)),
        usage_on: Default::default(),
    })))
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{bridge_url}/v1/messages"))
        .header("x-api-key", "anything")
        .json(&json!({
            "model": "claude-x", "max_tokens": 256,
            "messages": [{"role": "user", "content": "hi"}], "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let text = resp.text().await.unwrap();

    assert!(text.contains("event: message_start"), "got: {text}");
    assert!(text.contains("event: content_block_start"));
    assert!(text.contains("text_delta"));
    assert!(text.contains("Hello"));
    assert!(text.contains("world"));
    assert!(text.contains("event: message_delta"));
    assert!(text.contains("\"stop_reason\":\"end_turn\""));
    assert!(text.contains("event: message_stop"));
}

#[tokio::test]
async fn messages_non_streaming_end_to_end() {
    let upstream_url = spawn(Router::new().route("/chat/completions", post(mock_chat_json))).await;
    let cfg = messages_bridge_url_for(&upstream_url);
    let bridge_url = spawn(build_app(Arc::new(AppState {
        config: RwLock::new(Arc::new(cfg)),
        upstream: Upstream::new(),
        status: Default::default(),
        pool: None,
        watchers: Default::default(),
        usage: std::sync::Arc::new(ai_api_bridge::usage::UsageMeter::new(None)),
        usage_on: Default::default(),
    })))
    .await;

    let body: serde_json::Value = reqwest::Client::new()
        .post(format!("{bridge_url}/v1/messages"))
        .json(&json!({
            "model": "claude-x", "max_tokens": 256,
            "messages": [{"role": "user", "content": "hi"}], "stream": false
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][0]["text"], "Hello world");
    assert_eq!(body["stop_reason"], "end_turn");
}

#[tokio::test]
async fn messages_tool_use_streaming() {
    let upstream_url = spawn(Router::new().route("/chat/completions", post(mock_chat_tool))).await;
    let cfg = messages_bridge_url_for(&upstream_url);
    let bridge_url = spawn(build_app(Arc::new(AppState {
        config: RwLock::new(Arc::new(cfg)),
        upstream: Upstream::new(),
        status: Default::default(),
        pool: None,
        watchers: Default::default(),
        usage: std::sync::Arc::new(ai_api_bridge::usage::UsageMeter::new(None)),
        usage_on: Default::default(),
    })))
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{bridge_url}/v1/messages"))
        .json(&json!({
            "model": "claude-x", "max_tokens": 256,
            "messages": [{"role": "user", "content": "weather in SF?"}],
            "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    let text = resp.text().await.unwrap();

    assert!(text.contains("\"type\":\"tool_use\""), "got: {text}");
    assert!(text.contains("get_weather"));
    assert!(text.contains("input_json_delta"));
    assert!(text.contains("\"stop_reason\":\"tool_use\""));
}

#[tokio::test]
async fn messages_missing_model_returns_400() {
    let cfg = Config::from_toml("[providers.zen]\nwire=\"openai-chat\"\nbase_url=\"u\"").unwrap();
    let url = spawn(build_app(Arc::new(AppState {
        config: RwLock::new(Arc::new(cfg)),
        upstream: Upstream::new(),
        status: Default::default(),
        pool: None,
        watchers: Default::default(),
        usage: std::sync::Arc::new(ai_api_bridge::usage::UsageMeter::new(None)),
        usage_on: Default::default(),
    })))
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/messages"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

// --- reactive failover (per-request errors) ---

// Route gpt-5.5 -> `bad` (primary) with `good` as fallback; both providers point
// at the given mock upstreams.
fn failover_cfg(bad: &str, good: &str) -> Config {
    Config::from_toml(&format!(
        "[providers.bad]\nwire=\"openai-chat\"\nbase_url=\"{bad}\"\n\
         [providers.good]\nwire=\"openai-chat\"\nbase_url=\"{good}\"\n\
         [[routes]]\nalias=\"gpt-5.5\"\nprovider=\"bad\"\nmodel=\"x\"\n\
         fallback=[{{ provider=\"good\", model=\"deepseek-v4-pro\" }}]"
    ))
    .unwrap()
}

fn app(cfg: Config) -> Router {
    build_app(Arc::new(AppState {
        config: RwLock::new(Arc::new(cfg)),
        upstream: Upstream::new(),
        status: Default::default(),
        pool: None,
        watchers: Default::default(),
        usage: std::sync::Arc::new(ai_api_bridge::usage::UsageMeter::new(None)),
        usage_on: Default::default(),
    }))
}

#[tokio::test]
async fn reactive_failover_streaming_on_503() {
    let good = spawn(Router::new().route("/chat/completions", post(mock_chat))).await;
    let bad = spawn(Router::new().route("/chat/completions", post(mock_503))).await;
    let url = spawn(app(failover_cfg(&bad, &good))).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/responses"))
        .json(&json!({"model": "gpt-5.5", "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let text = resp.text().await.unwrap();
    // bad 503'd before any byte -> failed over to good's SSE
    assert!(text.contains("event: response.completed"), "got: {text}");
    assert!(text.contains("Hello"));
    assert!(text.contains("world"));
}

#[tokio::test]
async fn reactive_failover_non_streaming_on_503() {
    let good = spawn(Router::new().route("/chat/completions", post(mock_chat_json))).await;
    let bad = spawn(Router::new().route("/chat/completions", post(mock_503))).await;
    let url = spawn(app(failover_cfg(&bad, &good))).await;

    let body: serde_json::Value = reqwest::Client::new()
        .post(format!("{url}/v1/responses"))
        .json(&json!({"model": "gpt-5.5", "input": "hi", "stream": false}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"], "completed");
    assert_eq!(body["output"][0]["content"][0]["text"], "Hello world");
}

#[tokio::test]
async fn global_fallback_route_backs_routes_and_catches_unrouted_aliases() {
    let good = spawn(Router::new().route("/chat/completions", post(mock_chat_json))).await;
    let bad = spawn(Router::new().route("/chat/completions", post(mock_503))).await;
    // No default_provider; the route has no per-route fallback — only the global net.
    let cfg = Config::from_toml(&format!(
        "fallback_route = {{ provider = \"good\", model = \"deepseek-v4-pro\" }}\n\
         [providers.bad]\nwire=\"openai-chat\"\nbase_url=\"{bad}\"\n\
         [providers.good]\nwire=\"openai-chat\"\nbase_url=\"{good}\"\n\
         [[routes]]\nalias=\"gpt-5.5\"\nprovider=\"bad\"\nmodel=\"x\""
    ))
    .unwrap();
    let url = spawn(app(cfg)).await;
    let client = reqwest::Client::new();

    // Routed alias: primary 503s -> the global fallback serves the request.
    let body: serde_json::Value = client
        .post(format!("{url}/v1/responses"))
        .json(&json!({"model": "gpt-5.5", "input": "hi", "stream": false}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"], "completed");

    // Unrouted alias with no default_provider: caught by the global fallback.
    let body: serde_json::Value = client
        .post(format!("{url}/v1/responses"))
        .json(&json!({"model": "no-such-alias", "input": "hi", "stream": false}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"], "completed");
}

#[tokio::test]
async fn non_retryable_400_does_not_failover() {
    let good = spawn(Router::new().route("/chat/completions", post(mock_chat_json))).await;
    let bad = spawn(Router::new().route("/chat/completions", post(mock_400))).await;
    let url = spawn(app(failover_cfg(&bad, &good))).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/responses"))
        .json(&json!({"model": "gpt-5.5", "input": "hi", "stream": false}))
        .send()
        .await
        .unwrap();
    // bad returned 400 (not retryable) -> propagated, good never tried
    assert_eq!(resp.status(), 400);
}

// The cost/usage master switch gates recording: ON records the response's cost,
// OFF short-circuits (and leaves 429 failover untouched — tested separately).
#[tokio::test]
async fn usage_toggle_gates_recording() {
    use ai_api_bridge::config::{UsageKind, UsageWindow};

    let upstream_url =
        spawn(Router::new().route("/chat/completions", post(mock_chat_json_cost))).await;
    // zen carries a billing window (folds from cost_windows) so cost is recorded.
    let cfg = Config::from_toml(&format!(
        "default_provider=\"zen\"\n[providers.zen]\nwire=\"openai-chat\"\nbase_url=\"{upstream_url}\"\ncost_windows=[{{label=\"5h\",window_secs=18000,limit=100.0}}]"
    ))
    .unwrap();

    async fn spent_after_request(cfg: Config, on: bool) -> f64 {
        let state = Arc::new(AppState {
            config: RwLock::new(Arc::new(cfg)),
            upstream: Upstream::new(),
            status: Default::default(),
            pool: None,
            watchers: Default::default(),
            usage: std::sync::Arc::new(ai_api_bridge::usage::UsageMeter::new(None)),
            usage_on: std::sync::atomic::AtomicBool::new(on),
        });
        let usage = state.usage.clone();
        let url = spawn(build_app(state)).await;
        reqwest::Client::new()
            .post(format!("{url}/v1/chat/completions"))
            .json(&json!({"model": "m", "stream": false, "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let win = UsageWindow {
            label: "5h".into(),
            window_secs: 18000,
            limit: 100.0,
        };
        usage.windows("zen", UsageKind::Billing, &[win], now)[0].spent
    }

    // tracking ON -> the $0.50 cost is recorded for the served provider
    assert_eq!(spent_after_request(cfg.clone(), true).await, 0.5);
    // tracking OFF -> nothing recorded
    assert_eq!(spent_after_request(cfg, false).await, 0.0);
}

// A `count` usage spec increments by 1 per request (no cost needed).
#[tokio::test]
async fn count_usage_increments_per_request() {
    use ai_api_bridge::config::{UsageKind, UsageWindow};

    let upstream_url = spawn(Router::new().route("/chat/completions", post(mock_chat_json))).await;
    let cfg = Config::from_toml(&format!(
        "default_provider=\"zen\"\n[providers.zen]\nwire=\"openai-chat\"\nbase_url=\"{upstream_url}\"\n[[providers.zen.usage]]\nusage_type=\"count\"\nwindows=[{{label=\"1d\",window_secs=86400,limit=500.0}}]"
    ))
    .unwrap();
    // sanity: the count spec parsed
    assert_eq!(cfg.providers["zen"].usage.len(), 1);

    let state = Arc::new(AppState {
        config: RwLock::new(Arc::new(cfg)),
        upstream: Upstream::new(),
        status: Default::default(),
        pool: None,
        watchers: Default::default(),
        usage: std::sync::Arc::new(ai_api_bridge::usage::UsageMeter::new(None)),
        usage_on: std::sync::atomic::AtomicBool::new(true),
    });
    let usage = state.usage.clone();
    let url = spawn(build_app(state)).await;
    let client = reqwest::Client::new();
    for _ in 0..2 {
        client
            .post(format!("{url}/v1/chat/completions"))
            .json(&json!({"model": "m", "stream": false, "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let win = UsageWindow {
        label: "1d".into(),
        window_secs: 86400,
        limit: 500.0,
    };
    assert_eq!(
        usage.windows("zen", UsageKind::Count, &[win], now)[0].spent,
        2.0
    );
}
