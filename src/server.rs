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
