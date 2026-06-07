//! Admin REST API + embedded management page for CRUD-ing providers + routes.
//!
//! Endpoints live under `/admin` (kept off the `/v1/*` paths the AI clients use)
//! and reuse the bridge's `auth_token` via [`check_auth`]. Every successful write
//! calls [`reload_from_db`] so the change takes effect at runtime (config snapshot
//! swapped + watcher restarted) without a process restart.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Html;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::{CostWindow, ModelPrice, ProbeSource, Provider, Route, RouteTarget, UsageSpec, WireName};
use crate::error::BridgeError;
use crate::server::{AppState, check_auth, now_secs, reload_from_db};
use crate::store;

/// The embedded management page (single self-contained file, no runtime CDN).
const ADMIN_HTML: &str = include_str!("../web/admin.html");

/// `GET /` and `GET /admin` — serve the dashboard shell (the data behind it is
/// gated by the admin API's auth).
pub async fn page() -> Html<&'static str> {
    Html(ADMIN_HTML)
}

fn pool(state: &AppState) -> Result<&sqlx::SqlitePool, BridgeError> {
    state
        .pool
        .as_ref()
        .ok_or_else(|| BridgeError::Internal("no database handle".into()))
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ProviderInput {
    /// Only read on create (`POST`); on update the name comes from the path.
    #[serde(default)]
    pub name: Option<String>,
    pub wire: String,
    pub base_url: String,
    /// Blank/omitted on update preserves the stored key; a value replaces it.
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub model_prefix: Option<String>,
    #[serde(default)]
    pub max_tokens_field: Option<String>,
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
    #[serde(default)]
    pub probe_script: Option<String>,
    #[serde(default)]
    pub probe_script_text: Option<String>,
    #[serde(default)]
    pub probe_source: Option<String>,
    /// Explicit watcher on/off (null = auto: on when `probe_script` is set).
    #[serde(default)]
    pub probe_enabled: Option<bool>,
    #[serde(default)]
    pub probe_interval_secs: Option<u64>,
    #[serde(default)]
    pub quota_min: Option<f64>,
    #[serde(default)]
    pub cost_windows: Vec<CostWindow>,
    #[serde(default)]
    pub model_prices: HashMap<String, ModelPrice>,
    #[serde(default)]
    pub usage: Vec<UsageSpec>,
}

/// Drop empty strings to `None` so blank form fields don't persist as `Some("")`.
fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn parse_probe_source(s: Option<&str>) -> ProbeSource {
    match s {
        Some("text") => ProbeSource::Text,
        _ => ProbeSource::Path,
    }
}

impl ProviderInput {
    /// Build a `Provider`, validating wire + base_url. `existing_api_key` is folded
    /// in when the form leaves `api_key` blank (preserve-on-edit).
    fn into_provider(self, existing_api_key: Option<String>) -> Result<Provider, BridgeError> {
        let wire = WireName::parse(&self.wire).ok_or_else(|| {
            BridgeError::BadRequest(format!(
                "unknown wire '{}' (expected openai-chat | openai-responses | anthropic-messages)",
                self.wire
            ))
        })?;
        let base_url = self.base_url.trim().to_string();
        if base_url.is_empty() {
            return Err(BridgeError::BadRequest("base_url is required".into()));
        }
        let api_key = non_empty(self.api_key).or(existing_api_key);
        let mut provider = Provider {
            wire,
            base_url,
            api_key,
            model_prefix: non_empty(self.model_prefix),
            max_tokens_field: non_empty(self.max_tokens_field)
                .unwrap_or_else(|| "max_tokens".to_string()),
            extra_headers: self.extra_headers,
            probe_script: non_empty(self.probe_script),
            probe_script_text: non_empty(self.probe_script_text),
            probe_source: parse_probe_source(self.probe_source.as_deref()),
            probe_enabled: self.probe_enabled,
            probe_interval_secs: self.probe_interval_secs,
            quota_min: self.quota_min,
            cost_windows: self.cost_windows,
            model_prices: self.model_prices,
            usage: self.usage,
        };
        // Fold legacy cost_windows/model_prices into `usage` if the form sent them.
        provider.normalize_usage();
        Ok(provider)
    }
}

/// `GET /admin/api/providers` — full editable config + merged live status, with
/// `api_key` masked (returns `api_key_set` instead of the secret).
pub async fn list_providers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, BridgeError> {
    let cfg = state.config();
    check_auth(&cfg, &headers)?;

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
            // Typed usage view: per-spec {usage_type, unit, model_prices?, windows[]}
            // — config for the edit form + live spend for the bars (when tracking on).
            let usage = crate::server::usage_view(&state.usage, name, &p.usage, now, enabled);
            json!({
                "name": name,
                "wire": p.wire.as_str(),
                "base_url": p.base_url,
                "api_key_set": p.api_key.is_some(),
                "model_prefix": p.model_prefix,
                "max_tokens_field": p.max_tokens_field,
                "extra_headers": p.extra_headers,
                "probe_script": p.probe_script,
                "probe_script_text": p.probe_script_text,
                "probe_source": match p.probe_source { ProbeSource::Path => "path", ProbeSource::Text => "text" },
                "probe_enabled": p.probe_enabled(),
                "probe_enabled_override": p.probe_enabled,
                "probe_interval_secs": p.probe_interval_secs,
                "quota_min": p.quota_min,
                "usage": usage,
                "status": {
                    "available": s.available,
                    "quota_remaining": s.quota_remaining,
                    "quota_used": s.quota_used,
                    "quota_limit": s.quota_limit,
                    "last_checked": s.last_checked,
                    "last_ok": s.last_ok,
                    "error": s.error,
                    "note": s.note,
                },
            })
        })
        .collect();
    providers.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(Json(
        json!({ "providers": providers, "cost_tracking": enabled }),
    ))
}

/// `GET /admin/api/usage` — current cost/usage tracking state.
pub async fn get_usage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, BridgeError> {
    let cfg = state.config();
    check_auth(&cfg, &headers)?;
    Ok(Json(json!({ "enabled": state.usage_enabled() })))
}

#[derive(Deserialize)]
pub struct UsageToggle {
    pub enabled: bool,
}

/// `POST /admin/api/usage` — flip cost/usage tracking on/off at runtime. The change
/// is runtime-only; the persistent default is `cost_tracking` in the config file.
pub async fn set_usage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<UsageToggle>,
) -> Result<Json<Value>, BridgeError> {
    let cfg = state.config();
    check_auth(&cfg, &headers)?;
    state.set_usage_enabled(body.enabled);
    tracing::info!(
        enabled = body.enabled,
        "cost/usage tracking toggled via admin"
    );
    Ok(Json(json!({ "ok": true, "enabled": body.enabled })))
}

/// `POST /admin/api/providers` — create.
pub async fn create_provider(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<ProviderInput>,
) -> Result<Json<Value>, BridgeError> {
    let cfg = state.config();
    check_auth(&cfg, &headers)?;
    let pool = pool(&state)?;

    let name = non_empty(input.name.clone())
        .ok_or_else(|| BridgeError::BadRequest("name is required".into()))?;
    if store::get_provider(pool, &name)
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(BridgeError::Conflict(format!(
            "provider '{name}' already exists"
        )));
    }
    let provider = input.into_provider(None)?;
    store::insert_provider(pool, &name, &provider)
        .await
        .map_err(internal)?;
    reload_from_db(&state).await?;
    Ok(Json(json!({ "ok": true, "name": name })))
}

/// `PUT /admin/api/providers/:name` — update (name is the immutable PK).
pub async fn update_provider(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(input): Json<ProviderInput>,
) -> Result<Json<Value>, BridgeError> {
    let cfg = state.config();
    check_auth(&cfg, &headers)?;
    let pool = pool(&state)?;

    let existing = store::get_provider(pool, &name)
        .await
        .map_err(internal)?
        .ok_or_else(|| BridgeError::NotFound(format!("no such provider: {name}")))?;
    let provider = input.into_provider(existing.api_key)?;
    store::update_provider(pool, &name, &provider)
        .await
        .map_err(internal)?;
    reload_from_db(&state).await?;
    Ok(Json(json!({ "ok": true, "name": name })))
}

/// `DELETE /admin/api/providers/:name` — delete (cascades routes + status).
pub async fn delete_provider(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, BridgeError> {
    let cfg = state.config();
    check_auth(&cfg, &headers)?;
    let pool = pool(&state)?;

    let n = store::delete_provider(pool, &name)
        .await
        .map_err(internal)?;
    if n == 0 {
        return Err(BridgeError::NotFound(format!("no such provider: {name}")));
    }
    reload_from_db(&state).await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RouteInput {
    /// Only read on create; on update the alias comes from the path.
    #[serde(default)]
    pub alias: Option<String>,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub fallback: Vec<RouteTarget>,
}

impl RouteInput {
    /// Build a `Route`, validating that `provider` (and every fallback provider)
    /// exists and `model` is non-empty.
    fn into_route(
        self,
        alias: String,
        providers: &HashMap<String, Provider>,
    ) -> Result<Route, BridgeError> {
        let provider = self.provider.trim().to_string();
        let model = self.model.trim().to_string();
        if model.is_empty() {
            return Err(BridgeError::BadRequest("model is required".into()));
        }
        if !providers.contains_key(&provider) {
            return Err(BridgeError::BadRequest(format!(
                "unknown provider: {provider}"
            )));
        }
        for fb in &self.fallback {
            if !providers.contains_key(&fb.provider) {
                return Err(BridgeError::BadRequest(format!(
                    "unknown fallback provider: {}",
                    fb.provider
                )));
            }
        }
        Ok(Route {
            alias,
            provider,
            model,
            fallback: self.fallback,
        })
    }
}

/// `GET /admin/api/routes` — list all routes.
pub async fn list_routes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, BridgeError> {
    let cfg = state.config();
    check_auth(&cfg, &headers)?;

    let mut routes: Vec<Value> = cfg
        .routes
        .iter()
        .map(|r| {
            json!({
                "alias": r.alias,
                "provider": r.provider,
                "model": r.model,
                "fallback": r.fallback,
            })
        })
        .collect();
    routes.sort_by(|a, b| a["alias"].as_str().cmp(&b["alias"].as_str()));
    Ok(Json(json!({ "routes": routes })))
}

/// `POST /admin/api/routes` — create.
pub async fn create_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<RouteInput>,
) -> Result<Json<Value>, BridgeError> {
    let cfg = state.config();
    check_auth(&cfg, &headers)?;
    let pool = pool(&state)?;

    let alias = non_empty(input.alias.clone())
        .ok_or_else(|| BridgeError::BadRequest("alias is required".into()))?;
    if cfg.routes.iter().any(|r| r.alias == alias) {
        return Err(BridgeError::Conflict(format!(
            "route '{alias}' already exists"
        )));
    }
    let route = input.into_route(alias.clone(), &cfg.providers)?;
    store::insert_route(pool, &route).await.map_err(internal)?;
    reload_from_db(&state).await?;
    Ok(Json(json!({ "ok": true, "alias": alias })))
}

/// `PUT /admin/api/routes/:alias` — update (alias is the immutable PK).
pub async fn update_route(
    State(state): State<Arc<AppState>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Json(input): Json<RouteInput>,
) -> Result<Json<Value>, BridgeError> {
    let cfg = state.config();
    check_auth(&cfg, &headers)?;
    let pool = pool(&state)?;

    if !cfg.routes.iter().any(|r| r.alias == alias) {
        return Err(BridgeError::NotFound(format!("no such route: {alias}")));
    }
    let route = input.into_route(alias.clone(), &cfg.providers)?;
    store::update_route(pool, &alias, &route)
        .await
        .map_err(internal)?;
    reload_from_db(&state).await?;
    Ok(Json(json!({ "ok": true, "alias": alias })))
}

/// `DELETE /admin/api/routes/:alias` — delete.
pub async fn delete_route(
    State(state): State<Arc<AppState>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, BridgeError> {
    let cfg = state.config();
    check_auth(&cfg, &headers)?;
    let pool = pool(&state)?;

    let n = store::delete_route(pool, &alias).await.map_err(internal)?;
    if n == 0 {
        return Err(BridgeError::NotFound(format!("no such route: {alias}")));
    }
    reload_from_db(&state).await?;
    Ok(Json(json!({ "ok": true })))
}

fn internal(e: anyhow::Error) -> BridgeError {
    BridgeError::Internal(e.to_string())
}
