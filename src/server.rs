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
use sqlx::sqlite::SqlitePool;

use crate::canonical::{CanonicalEvent, CanonicalRequest};
use crate::config::Config;
use crate::error::BridgeError;
use crate::router::{self, Resolved};
use crate::sse::{SseDecoder, SseItem};
use crate::upstream::{ByteStream, Upstream};
use crate::watcher::StatusMap;
use crate::wire::{CanonicalEmitter, anthropic, chat, responses};
use crate::{probe, store};

pub struct AppState {
    pub config: Config,
    pub upstream: Upstream,
    pub status: StatusMap,
    /// Live DB handle for reactive re-probes (None in tests).
    pub pool: Option<SqlitePool>,
}

pub fn build_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/responses", post(responses_handler))
        .route("/v1/chat/completions", post(chat_handler))
        .route("/v1/models", get(list_models))
        .route("/v1/messages", post(messages_handler))
        .route("/v1/providers", get(providers_status))
        .with_state(state)
}

/// Watcher view of each provider: availability + quota + last-check info.
async fn providers_status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let status = state.status.read().ok();
    let mut providers: Vec<Value> = state
        .config
        .providers
        .iter()
        .map(|(name, p)| {
            let s = status
                .as_ref()
                .and_then(|m| m.get(name))
                .cloned()
                .unwrap_or_default();
            json!({
                "name": name,
                "base_url": p.base_url,
                "probe_enabled": p.probe_enabled(),
                "available": s.available,
                "quota_remaining": s.quota_remaining,
                "quota_used": s.quota_used,
                "quota_limit": s.quota_limit,
                "quota_min": p.quota_min,
                "last_checked": s.last_checked,
                "last_ok": s.last_ok,
                "error": s.error,
                "note": s.note,
            })
        })
        .collect();
    providers.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Json(json!({ "providers": providers }))
}

async fn health() -> &'static str {
    "ok"
}

fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), BridgeError> {
    let Some(expected) = &state.config.auth_token else {
        return Ok(());
    };
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    // Anthropic clients (e.g. Claude Code) send the key via `x-api-key`.
    let api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    if bearer == Some(expected.as_str()) || api_key == Some(expected.as_str()) {
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
    let response_id = format!("resp_{}", uuid::Uuid::new_v4().simple());
    let candidates = {
        let status = state.status.read().unwrap_or_else(|e| e.into_inner());
        router::resolve_candidates(&state.config, &status, &req.model)?
    };
    tracing::info!(model = %req.model, candidates = candidates.len(), stream = req.stream, "responses request");

    if req.stream {
        let byte_stream = open_upstream_stream(&state, &req, &candidates).await?;
        Ok(
            run_stream(byte_stream, response_id, responses::ResponsesEmitter::new())
                .into_response(),
        )
    } else {
        let upstream_resp = call_upstream_json(&state, &req, &candidates).await?;
        let events = chat::completion_to_events(&upstream_resp, &response_id);
        let mut emitter = responses::ResponsesEmitter::new();
        for ev in &events {
            emitter.on_event(ev);
        }
        Ok(Json(emitter.final_response()).into_response())
    }
}

/// Drive the upstream Chat Completions stream through the canonical pipeline and
/// out as the client's SSE dialect (Responses or Anthropic Messages), chosen by
/// the `emitter` passed in.
fn run_stream<E: CanonicalEmitter + Send + 'static>(
    byte_stream: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    response_id: String,
    mut emitter: E,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let s = stream! {
        let mut decoder = SseDecoder::default();
        let mut parser = chat::ChatStreamParser::new(response_id);
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
                                    for fr in emitter.emit(&cev) {
                                        yield Ok(Event::default().event(fr.event).data(fr.data.to_string()));
                                    }
                                    if is_error {
                                        completed = true;
                                        break 'outer;
                                    }
                                }
                            }
                            SseItem::Done => {
                                for fr in emitter.emit(&CanonicalEvent::Completed) {
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
                    for fr in emitter.emit(&ev) {
                        yield Ok(Event::default().event(fr.event).data(fr.data.to_string()));
                    }
                    completed = true;
                    break;
                }
            }
        }

        if !completed {
            for fr in emitter.emit(&CanonicalEvent::Completed) {
                yield Ok(Event::default().event(fr.event).data(fr.data.to_string()));
            }
        }
    };
    Sse::new(s)
}

/// Whether an upstream failure should advance to the next fallback candidate.
/// (Connection/timeout, 5xx, 429 rate-limit, 401/402 quota — but not other 4xx,
/// which are bad-request errors that retrying elsewhere won't fix.)
fn is_retryable(e: &BridgeError) -> bool {
    match e {
        BridgeError::UpstreamUnreachable(_) => true,
        BridgeError::Upstream { status, .. } => {
            matches!(status, 401 | 402 | 408 | 429) || (500..=599).contains(status)
        }
        _ => false,
    }
}

/// React to a candidate failing: mark it unavailable in the in-memory status map
/// now (so concurrent/subsequent requests skip it), and for monitored providers
/// kick off a one-shot re-probe that writes the corrected status to map + DB.
fn mark_degraded(state: &AppState, cand: &Resolved<'_>, err: &BridgeError) {
    if let Ok(mut m) = state.status.write() {
        let entry = m.entry(cand.provider_name.clone()).or_default();
        entry.available = false;
        entry.last_ok = false;
        entry.error = Some(format!("request failed: {err}"));
    }
    if cand.provider.probe_enabled()
        && let Some(pool) = state.pool.clone()
    {
        let name = cand.provider_name.clone();
        let provider = cand.provider.clone();
        let status = state.status.clone();
        tokio::spawn(async move {
            let s = probe::run_probe(&name, &provider).await;
            let _ = store::write_status(&pool, &name, &s).await;
            if let Ok(mut m) = status.write() {
                m.insert(name, s);
            }
        });
    }
}

/// Try each candidate's upstream in order until one accepts (2xx), returning the
/// live byte stream. Response headers arrive before any body byte, so a failure
/// here means nothing was sent to the client yet — the only safe retry point for
/// streaming. Once this returns Ok, the caller is committed to that stream.
async fn open_upstream_stream(
    state: &AppState,
    req: &CanonicalRequest,
    candidates: &[Resolved<'_>],
) -> Result<ByteStream, BridgeError> {
    let mut last_err: Option<BridgeError> = None;
    let n = candidates.len();
    for (i, cand) in candidates.iter().enumerate() {
        let chat_body = chat::build_request(req, &cand.upstream_model, cand.provider);
        match state
            .upstream
            .post_stream(cand.provider, "/chat/completions", &chat_body)
            .await
        {
            Ok(stream) => {
                if i > 0 {
                    tracing::info!(provider = %cand.provider_name, attempt = i + 1, "reactive failover: upstream accepted");
                }
                return Ok(stream);
            }
            Err(e) if is_retryable(&e) && i + 1 < n => {
                tracing::warn!(provider = %cand.provider_name, "upstream failed, trying next: {e}");
                mark_degraded(state, cand, &e);
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| BridgeError::Internal("no upstream candidates".into())))
}

/// Non-streaming counterpart of `open_upstream_stream`.
async fn call_upstream_json(
    state: &AppState,
    req: &CanonicalRequest,
    candidates: &[Resolved<'_>],
) -> Result<Value, BridgeError> {
    let mut last_err: Option<BridgeError> = None;
    let n = candidates.len();
    for (i, cand) in candidates.iter().enumerate() {
        let chat_body = chat::build_request(req, &cand.upstream_model, cand.provider);
        match state
            .upstream
            .post_json(cand.provider, "/chat/completions", &chat_body)
            .await
        {
            Ok(json) => {
                if i > 0 {
                    tracing::info!(provider = %cand.provider_name, attempt = i + 1, "reactive failover: upstream accepted");
                }
                return Ok(json);
            }
            Err(e) if is_retryable(&e) && i + 1 < n => {
                tracing::warn!(provider = %cand.provider_name, "upstream failed, trying next: {e}");
                mark_degraded(state, cand, &e);
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| BridgeError::Internal("no upstream candidates".into())))
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
    let candidates = {
        let status = state.status.read().unwrap_or_else(|e| e.into_inner());
        router::resolve_candidates(&state.config, &status, &req.model)?
    };

    if req.stream {
        let byte_stream = open_upstream_stream(&state, &req, &candidates).await?;
        let resp = Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(byte_stream))
            .map_err(|e| BridgeError::Internal(e.to_string()))?;
        Ok(resp)
    } else {
        let resp = call_upstream_json(&state, &req, &candidates).await?;
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

/// Anthropic Messages inbound (for Claude Code via `ANTHROPIC_BASE_URL`): parse
/// → route → call the Chat Completions upstream → translate the response back to
/// Anthropic Messages (streaming SSE or a single message object).
async fn messages_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, BridgeError> {
    check_auth(&state, &headers)?;

    let req = anthropic::parse_request(&body)?;
    let response_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
    let candidates = {
        let status = state.status.read().unwrap_or_else(|e| e.into_inner());
        router::resolve_candidates(&state.config, &status, &req.model)?
    };
    tracing::info!(model = %req.model, candidates = candidates.len(), stream = req.stream, "messages request");

    if req.stream {
        let byte_stream = open_upstream_stream(&state, &req, &candidates).await?;
        Ok(
            run_stream(byte_stream, response_id, anthropic::AnthropicEmitter::new())
                .into_response(),
        )
    } else {
        let upstream_resp = call_upstream_json(&state, &req, &candidates).await?;
        let events = chat::completion_to_events(&upstream_resp, &response_id);
        let mut emitter = anthropic::AnthropicEmitter::new();
        for ev in &events {
            emitter.on_event(ev);
        }
        Ok(Json(emitter.final_message()).into_response())
    }
}
