//! axum app + handlers + the streaming translation pipeline.

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use async_stream::stream;
use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};
use sqlx::sqlite::SqlitePool;

use crate::canonical::{CanonicalEvent, CanonicalRequest};
use crate::config::{Config, ModelPrice, UsageKind, UsageSpec};
use crate::error::BridgeError;
use crate::router::{self, Resolved};
use crate::sse::{SseDecoder, SseItem};
use crate::upstream::{ByteStream, Upstream};
use crate::usage::UsageMeter;
use crate::watcher::{self, StatusMap, WatcherHandles};
use crate::wire::{CanonicalEmitter, anthropic, chat, responses};
use crate::{probe, store, usage};

pub struct AppState {
    /// The live provider/route config. Wrapped in `RwLock<Arc<_>>` so admin CRUD
    /// can hot-swap it: handlers take a cheap `Arc` snapshot at entry (dropping the
    /// guard before any `.await`), so `Resolved<'a>` never borrows across a lock.
    pub config: RwLock<Arc<Config>>,
    pub upstream: Upstream,
    pub status: StatusMap,
    /// Live DB handle for reactive re-probes + admin writes (None in tests).
    pub pool: Option<SqlitePool>,
    /// Running probe-task handles, aborted + respawned on a provider change.
    pub watchers: WatcherHandles,
    /// Rolling per-provider cost accumulator (windows + failover input).
    pub usage: Arc<UsageMeter>,
    /// Master switch for all cost/usage tracking (seeded from `cost_tracking`).
    /// When off, every usage path short-circuits; 429 failover is unaffected.
    pub usage_on: AtomicBool,
}

/// Records the served provider's usage when a (streaming) response finishes. Holds
/// the provider's usage kinds + (for billing) the served model's price, so amounts
/// can be computed at stream end.
struct CostRecorder {
    meter: Arc<UsageMeter>,
    provider: String,
    kinds: Vec<(UsageKind, Option<ModelPrice>)>,
}

/// The provider's usage kinds paired with the served model's billing price (if any) —
/// everything needed to compute per-kind amounts for one request.
fn usage_kinds(cfg: &Config, provider: &str, model: &str) -> Vec<(UsageKind, Option<ModelPrice>)> {
    cfg.providers
        .get(provider)
        .map(|p| {
            p.usage
                .iter()
                .map(|spec| {
                    let price = spec.model_prices().and_then(|m| m.get(model)).copied();
                    (spec.kind(), price)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Record one request's amount for each of the provider's usage kinds.
async fn record_amounts(
    meter: &UsageMeter,
    provider: &str,
    kinds: &[(UsageKind, Option<ModelPrice>)],
    real: Option<f64>,
    pt: Option<u64>,
    ct: Option<u64>,
) {
    let now = now_secs();
    for (kind, price) in kinds {
        if let Some(amount) = usage::amount_for(*kind, price.as_ref(), real, pt, ct) {
            meter.record(provider, *kind, amount, now).await;
        }
    }
}

/// Per-provider usage view for `/v1/providers` + admin: one entry per usage spec,
/// `{usage_type, unit, [model_prices], windows[]}`. When `enabled`, windows carry
/// live `spent/remaining/reset_in_secs`; otherwise just the config (label/secs/limit).
pub(crate) fn usage_view(
    meter: &UsageMeter,
    provider: &str,
    specs: &[UsageSpec],
    now: i64,
    enabled: bool,
) -> Vec<Value> {
    specs
        .iter()
        .map(|spec| {
            let kind = spec.kind();
            let windows: Vec<Value> = if enabled {
                let stats = meter.windows(provider, kind, spec.windows(), now);
                spec.windows()
                    .iter()
                    .zip(stats)
                    .map(|(w, st)| {
                        json!({
                            "label": w.label, "window_secs": w.window_secs, "limit": w.limit,
                            "spent": st.spent, "remaining": st.remaining,
                            "reset_in_secs": st.reset_in_secs,
                        })
                    })
                    .collect()
            } else {
                spec.windows()
                    .iter()
                    .map(|w| json!({ "label": w.label, "window_secs": w.window_secs, "limit": w.limit }))
                    .collect()
            };
            let mut obj = json!({ "usage_type": kind.as_str(), "unit": kind.unit(), "windows": windows });
            if let Some(mp) = spec.model_prices() {
                obj["model_prices"] = json!(mp);
            }
            obj
        })
        .collect()
}

/// Build a stream usage recorder — `None` when tracking is off (so `run_stream`
/// skips all capture) or the provider has no usage specs.
fn cost_recorder(
    state: &AppState,
    cfg: &Config,
    provider: String,
    model: &str,
) -> Option<CostRecorder> {
    if !state.usage_enabled() {
        return None;
    }
    let kinds = usage_kinds(cfg, &provider, model);
    if kinds.is_empty() {
        return None;
    }
    Some(CostRecorder {
        meter: state.usage.clone(),
        provider,
        kinds,
    })
}

/// Current unix time in seconds (for cost-event timestamps).
pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl AppState {
    /// Cheap snapshot of the current config (clones an `Arc`, not the `Config`).
    pub fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether cost/usage tracking is currently on.
    pub fn usage_enabled(&self) -> bool {
        self.usage_on.load(Ordering::Relaxed)
    }

    /// Flip cost/usage tracking on/off at runtime (admin toggle).
    pub fn set_usage_enabled(&self, on: bool) {
        self.usage_on.store(on, Ordering::Relaxed);
    }
}

/// Rebuild the live config from the DB and re-sync the runtime after an admin
/// provider/route write: reload providers+routes, swap the config snapshot,
/// restart the probe tasks (so the watcher tracks added/edited/removed providers),
/// and drop status entries for providers that no longer exist.
pub(crate) async fn reload_from_db(state: &AppState) -> Result<(), BridgeError> {
    let Some(pool) = &state.pool else {
        return Err(BridgeError::Internal("no database handle".into()));
    };
    // Keep process-level fields (listen/database/default_provider/auth_token) from
    // the current snapshot; providers + routes + the fallback-route setting are
    // reloaded from the DB.
    let mut cfg = (*state.config()).clone();
    store::load_into_config(pool, &mut cfg)
        .await
        .map_err(|e| BridgeError::Internal(format!("reload from db: {e}")))?;
    cfg.apply_env_overrides();
    let providers = cfg.providers.clone();

    *state.config.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(cfg);
    watcher::reconcile(
        &state.watchers,
        pool.clone(),
        &providers,
        state.status.clone(),
    );
    if let Ok(mut m) = state.status.write() {
        m.retain(|name, _| providers.contains_key(name));
    }
    Ok(())
}

pub fn build_app(state: Arc<AppState>) -> Router {
    use crate::admin;
    Router::new()
        .route("/health", get(health))
        .route("/v1/responses", post(responses_handler))
        .route("/v1/chat/completions", post(chat_handler))
        .route("/v1/models", get(list_models))
        .route("/v1/messages", post(messages_handler))
        .route("/v1/providers", get(providers_status))
        // Admin: management page + provider/route CRUD (reuses `auth_token`).
        .route("/", get(admin::page))
        .route("/admin", get(admin::page))
        .route(
            "/admin/api/providers",
            get(admin::list_providers).post(admin::create_provider),
        )
        .route(
            "/admin/api/providers/:name",
            put(admin::update_provider).delete(admin::delete_provider),
        )
        .route(
            "/admin/api/routes",
            get(admin::list_routes).post(admin::create_route),
        )
        .route(
            "/admin/api/routes/:alias",
            put(admin::update_route).delete(admin::delete_route),
        )
        // Global fallback route (safety net appended to every candidate chain).
        .route(
            "/admin/api/fallback_route",
            get(admin::get_fallback_route).put(admin::set_fallback_route),
        )
        // Cost/usage tracking master switch.
        .route(
            "/admin/api/usage",
            get(admin::get_usage).post(admin::set_usage),
        )
        .route("/admin/api/probes", get(admin::list_probe_files))
        .with_state(state)
}

/// Watcher view of each provider: availability + quota + last-check info.
async fn providers_status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let cfg = state.config();
    let status = state.status.read().ok();
    let now = now_secs();
    let enabled = state.usage_enabled();
    let mut providers: Vec<Value> = cfg
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
                "usage": usage_view(&state.usage, name, &p.usage, now, enabled),
            })
        })
        .collect();
    providers.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Json(json!({ "providers": providers, "cost_tracking": enabled }))
}

async fn health() -> &'static str {
    "ok"
}

pub(crate) fn check_auth(cfg: &Config, headers: &HeaderMap) -> Result<(), BridgeError> {
    let Some(expected) = &cfg.auth_token else {
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
    let cfg = state.config();
    check_auth(&cfg, &headers)?;

    let req = responses::parse_request(&body)?;
    let response_id = format!("resp_{}", uuid::Uuid::new_v4().simple());
    let candidates = {
        let status = state.status.read().unwrap_or_else(|e| e.into_inner());
        // Usage-based demotion only when tracking is on; 429 failover is separate.
        let exhausted = if state.usage_enabled() {
            state.usage.exhausted_set(&cfg.providers, now_secs())
        } else {
            std::collections::HashSet::new()
        };
        router::resolve_candidates(&cfg, &status, &exhausted, &req.model)?
    };
    tracing::info!(model = %req.model, candidates = candidates.len(), stream = req.stream, provider = %candidates[0].provider_name, "responses request");

    if req.stream {
        let (provider, model, byte_stream) =
            open_upstream_stream(&state, &req, &candidates).await?;
        let recorder = cost_recorder(&state, &cfg, provider, &model);
        Ok(run_stream(
            byte_stream,
            response_id,
            responses::ResponsesEmitter::new(),
            recorder,
        )
        .into_response())
    } else {
        let (provider, model, upstream_resp) =
            call_upstream_json(&state, &req, &candidates).await?;
        record_nonstream(&state, &cfg, &provider, &model, &upstream_resp).await;
        let events = chat::completion_to_events(&upstream_resp, &response_id);
        let mut emitter = responses::ResponsesEmitter::new();
        for ev in &events {
            emitter.on_event(ev);
        }
        Ok(Json(emitter.final_response()).into_response())
    }
}

/// Record a non-stream response's usage against the served provider, per kind.
/// No-op when tracking is off or the provider has no usage specs.
async fn record_nonstream(
    state: &AppState,
    cfg: &Config,
    provider: &str,
    model: &str,
    resp: &Value,
) {
    if !state.usage_enabled() {
        return;
    }
    let kinds = usage_kinds(cfg, provider, model);
    if kinds.is_empty() {
        return;
    }
    let real = resp.get("cost").and_then(usage::parse_cost);
    let (pt, ct) = usage::tokens_from_usage(resp.get("usage"));
    record_amounts(&state.usage, provider, &kinds, real, pt, ct).await;
}

/// Drive the upstream Chat Completions stream through the canonical pipeline and
/// out as the client's SSE dialect (Responses or Anthropic Messages), chosen by
/// the `emitter` passed in.
fn run_stream<E: CanonicalEmitter + Send + 'static>(
    byte_stream: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    response_id: String,
    mut emitter: E,
    recorder: Option<CostRecorder>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Tracking off (`recorder == None`) ⇒ no cost/token capture and no post-[DONE]
    // drain: the stream behaves exactly as it did before the cost accumulator.
    let track = recorder.is_some();
    let s = stream! {
        let mut decoder = SseDecoder::default();
        let mut parser = chat::ChatStreamParser::new(response_id);
        let mut completed = false;
        // After `[DONE]` we keep draining the upstream (without yielding more client
        // frames) only to catch the trailing `{"cost":...}` chunk Zen sends last.
        let mut done = false;
        let mut cost: Option<f64> = None;
        // Token usage (from the `usage` chunk) for the price estimate when cost is $0.
        let mut ptok: Option<u64> = None;
        let mut ctok: Option<u64> = None;

        futures_util::pin_mut!(byte_stream);
        'outer: while let Some(chunk) = byte_stream.next().await {
            match chunk {
                Ok(bytes) => {
                    for item in decoder.push(&bytes) {
                        match item {
                            SseItem::Data(d) => {
                                let Ok(json) = serde_json::from_str::<Value>(&d) else { continue };
                                if track {
                                    // Capture cost (Zen sends it after [DONE]) + token usage.
                                    if let Some(c) = json.get("cost").and_then(usage::parse_cost) {
                                        cost = Some(c);
                                    }
                                    let (pt, ct) = usage::tokens_from_usage(json.get("usage"));
                                    if pt.is_some() { ptok = pt; }
                                    if ct.is_some() { ctok = ct; }
                                }
                                if done {
                                    continue; // past [DONE]: sniff cost only, don't emit
                                }
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
                                if track {
                                    done = true; // keep draining to catch the trailing cost chunk
                                } else {
                                    break 'outer; // tracking off: stop at [DONE], no drain
                                }
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

        if let Some(r) = &recorder {
            record_amounts(&r.meter, &r.provider, &r.kinds, cost, ptok, ctok).await;
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
) -> Result<(String, String, ByteStream), BridgeError> {
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
                return Ok((
                    cand.provider_name.clone(),
                    cand.upstream_model.clone(),
                    stream,
                ));
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
) -> Result<(String, String, Value), BridgeError> {
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
                return Ok((
                    cand.provider_name.clone(),
                    cand.upstream_model.clone(),
                    json,
                ));
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
    let cfg = state.config();
    check_auth(&cfg, &headers)?;
    let req = chat::parse_request(&body)?;
    let candidates = {
        let status = state.status.read().unwrap_or_else(|e| e.into_inner());
        // Usage-based demotion only when tracking is on; 429 failover is separate.
        let exhausted = if state.usage_enabled() {
            state.usage.exhausted_set(&cfg.providers, now_secs())
        } else {
            std::collections::HashSet::new()
        };
        router::resolve_candidates(&cfg, &status, &exhausted, &req.model)?
    };

    if req.stream {
        // Passthrough streaming forwards bytes verbatim; cost is not captured here
        // (the trailing post-[DONE] cost chunk isn't parsed on this path — by design).
        let (_provider, _model, byte_stream) =
            open_upstream_stream(&state, &req, &candidates).await?;
        let resp = Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(byte_stream))
            .map_err(|e| BridgeError::Internal(e.to_string()))?;
        Ok(resp)
    } else {
        let (provider, model, resp) = call_upstream_json(&state, &req, &candidates).await?;
        record_nonstream(&state, &cfg, &provider, &model, &resp).await;
        Ok(Json(resp).into_response())
    }
}

async fn list_models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let cfg = state.config();
    let data: Vec<Value> = cfg
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
    let cfg = state.config();
    check_auth(&cfg, &headers)?;

    let req = anthropic::parse_request(&body)?;
    let response_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
    let candidates = {
        let status = state.status.read().unwrap_or_else(|e| e.into_inner());
        // Usage-based demotion only when tracking is on; 429 failover is separate.
        let exhausted = if state.usage_enabled() {
            state.usage.exhausted_set(&cfg.providers, now_secs())
        } else {
            std::collections::HashSet::new()
        };
        router::resolve_candidates(&cfg, &status, &exhausted, &req.model)?
    };
    tracing::info!(model = %req.model, candidates = candidates.len(), stream = req.stream, provider = %candidates[0].provider_name, "messages request");

    if req.stream {
        let (provider, model, byte_stream) =
            open_upstream_stream(&state, &req, &candidates).await?;
        let recorder = cost_recorder(&state, &cfg, provider, &model);
        Ok(run_stream(
            byte_stream,
            response_id,
            anthropic::AnthropicEmitter::new(),
            recorder,
        )
        .into_response())
    } else {
        let (provider, model, upstream_resp) =
            call_upstream_json(&state, &req, &candidates).await?;
        record_nonstream(&state, &cfg, &provider, &model, &upstream_resp).await;
        let events = chat::completion_to_events(&upstream_resp, &response_id);
        let mut emitter = anthropic::AnthropicEmitter::new();
        for ev in &events {
            emitter.on_event(ev);
        }
        Ok(Json(emitter.final_message()).into_response())
    }
}
