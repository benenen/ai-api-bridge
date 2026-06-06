//! Background provider watcher: periodically probes each enabled provider and
//! writes its status to the DB + a shared in-memory map (read by the status
//! endpoint and the failover router).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use sqlx::sqlite::SqlitePool;
use tokio::task::JoinHandle;

use crate::config::Provider;
use crate::probe::run_probe;
use crate::store::{self, ProviderStatus};

/// Live provider status shared between the watcher, the router, and the endpoint.
pub type StatusMap = Arc<RwLock<HashMap<String, ProviderStatus>>>;

/// Handles to the running probe tasks, so they can be aborted + respawned when an
/// admin CRUD change alters the provider set (see [`reconcile`]).
pub type WatcherHandles = Mutex<Vec<JoinHandle<()>>>;

/// Spawn one background task per probe-enabled provider. Each probes immediately,
/// then on its interval, persisting to the DB + the shared map. Returns the task
/// handles so they can later be aborted by [`reconcile`].
pub fn spawn(
    pool: SqlitePool,
    providers: &HashMap<String, Provider>,
    status: StatusMap,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    for (name, provider) in providers {
        if !provider.probe_enabled() {
            continue;
        }
        let name = name.clone();
        let provider = provider.clone();
        let pool = pool.clone();
        let status = status.clone();
        handles.push(tokio::spawn(watch_provider(name, provider, pool, status)));
    }
    handles
}

/// Abort the current probe tasks and respawn from `providers`. Called after an
/// admin provider write so the watcher tracks the new config (added / edited probe
/// settings / removed). Abort-all + respawn is fine: CRUD is human-paced and rare.
pub fn reconcile(
    handles: &WatcherHandles,
    pool: SqlitePool,
    providers: &HashMap<String, Provider>,
    status: StatusMap,
) {
    let mut guard = handles.lock().unwrap_or_else(|e| e.into_inner());
    for h in guard.drain(..) {
        h.abort();
    }
    *guard = spawn(pool, providers, status);
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
