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
    let bridge_url = spawn(build_app(Arc::new(AppState { config: cfg, upstream: Upstream::new() }))).await;

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
