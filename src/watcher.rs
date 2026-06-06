//! Background provider watcher: periodically probes each enabled provider and
//! writes its status to the DB + a shared in-memory map (read by the status
//! endpoint and the failover router).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use sqlx::sqlite::SqlitePool;

use crate::config::Provider;
use crate::probe::run_probe;
use crate::store::{self, ProviderStatus};

/// Live provider status shared between the watcher, the router, and the endpoint.
pub type StatusMap = Arc<RwLock<HashMap<String, ProviderStatus>>>;

/// Spawn one background task per probe-enabled provider. Each probes immediately,
/// then on its interval, persisting to the DB + the shared map.
pub fn spawn(pool: SqlitePool, providers: &HashMap<String, Provider>, status: StatusMap) {
    for (name, provider) in providers {
        if !provider.probe_enabled() {
            continue;
        }
        let name = name.clone();
        let provider = provider.clone();
        let pool = pool.clone();
        let status = status.clone();
        tokio::spawn(watch_provider(name, provider, pool, status));
    }
}

async fn watch_provider(name: String, provider: Provider, pool: SqlitePool, status: StatusMap) {
    let mut ticker = tokio::time::interval(provider.probe_interval());
    loop {
        ticker.tick().await; // fires immediately on the first iteration
        let new = run_probe(&name, &provider).await;

        let prev_available = status
            .read()
            .ok()
            .and_then(|m| m.get(&name).map(|s| s.available));
        if prev_available != Some(new.available) {
            if new.available {
                tracing::info!(provider = %name, remaining = ?new.quota_remaining, "provider available");
            } else {
                tracing::warn!(provider = %name, error = ?new.error, "provider UNAVAILABLE");
            }
        }

        if let Err(e) = store::write_status(&pool, &name, &new).await {
            tracing::warn!(provider = %name, "failed to persist provider status: {e}");
        }
        if let Ok(mut m) = status.write() {
            m.insert(name.clone(), new);
        }
    }
}
