//! axum app + handlers + the streaming translation pipeline.

use std::convert::Infallible;
use std::sync::Arc;

use async_stream::stream;
use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};

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
        .route("/v1/chat/completions", post(chat_handler))
        .route("/v1/models", get(list_models))
        .route("/v1/messages", post(messages_handler))
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
        Err(BridgeError::Unauthorized(
            "invalid bridge auth token".into(),
        ))
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
        'outer: while let Some(chunk) = byte_stream.next().await {
            match chunk {
                Ok(bytes) => {
                    for item in decoder.push(&bytes) {
                        match item {
                            SseItem::Data(d) => {
                                let Ok(json) = serde_json::from_str::<Value>(&d) else { continue };
                                for cev in parser.on_chunk(&json) {
                                    let is_error = matches!(cev, CanonicalEvent::Error { .. });
                                    for fr in emitter.on_event(&cev) {
                                        yield Ok(Event::default().event(fr.event).data(fr.data.to_string()));
                                    }
                                    if is_error {
                                        completed = true;
                                        break 'outer;
                                    }
                                }
                            }
                            SseItem::Done => {
                                for fr in emitter.on_event(&CanonicalEvent::Completed) {
                                    yield Ok(Event::default().event(fr.event).data(fr.data.to_string()));
                                }
                                completed = true;
                                break 'outer;
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

/// Chat Completions inbound: parse → route → call upstream → pass the response
/// straight through (inbound and outbound are both Chat Completions, so no
/// response translation is needed; streaming bytes are forwarded verbatim).
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
        let byte_stream = state
            .upstream
            .post_stream(resolved.provider, "/chat/completions", &chat_body)
            .await?;
        let resp = Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(byte_stream))
            .map_err(|e| BridgeError::Internal(e.to_string()))?;
        Ok(resp)
    } else {
        let resp = state
            .upstream
            .post_json(resolved.provider, "/chat/completions", &chat_body)
            .await?;
        Ok(Json(resp).into_response())
    }
}

async fn list_models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let data: Vec<Value> = state
        .config
        .routes
        .iter()
        .map(|r| json!({"id": r.alias, "object": "model", "owned_by": r.provider}))
        .collect();
    Json(json!({"object": "list", "data": data}))
}

async fn messages_handler() -> Result<Response, BridgeError> {
    Err(crate::wire::anthropic::not_implemented())
}
