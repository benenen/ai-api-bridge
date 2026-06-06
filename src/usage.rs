//! Cost accumulator: per-provider rolling spend windows.
//!
//! Some providers (notably OpenCode Zen's Go plan) expose no usage API — only a
//! per-request `cost` field on each completion. The bridge sums that into rolling
//! windows (Go: $12/5h, $30/7d, $60/30d) so it can show "remaining" and fail over
//! *before* a window is exhausted. The in-memory event log is authoritative for
//! reads (status + failover); SQLite persists it across restarts.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde_json::Value;
use sqlx::sqlite::SqlitePool;

use crate::config::{CostWindow, ModelPrice, Provider};
use crate::store;

/// Retention horizon: events older than this are dropped (≥ the largest standard
/// window of 30d, with a day of slack).
const RETAIN_SECS: i64 = 31 * 24 * 3600;

/// Per-window spend snapshot (exposed at `/v1/providers` + the admin page).
#[derive(Debug, Clone, serde::Serialize)]
pub struct WindowStat {
    pub label: String,
    pub limit: f64,
    pub spent: f64,
    pub remaining: f64,
    /// Seconds until the oldest in-window event ages out (soonest partial relief).
    pub reset_in_secs: i64,
}

/// In-memory rolling spend log per provider, persisted to SQLite.
pub struct UsageMeter {
    /// provider -> (ts_secs, cost), kept ascending by ts.
    events: Mutex<HashMap<String, Vec<(i64, f64)>>>,
    pool: Option<SqlitePool>,
}

impl UsageMeter {
    pub fn new(pool: Option<SqlitePool>) -> Self {
        Self {
            events: Mutex::new(HashMap::new()),
            pool,
        }
    }

    /// Seed the in-memory log from persisted events (call once at startup).
    pub fn load(&self, events: Vec<(String, i64, f64)>) {
        let mut map = self.events.lock().unwrap_or_else(|e| e.into_inner());
        for (provider, ts, cost) in events {
            map.entry(provider).or_default().push((ts, cost));
        }
        for v in map.values_mut() {
            v.sort_by_key(|(ts, _)| *ts);
        }
    }

    /// Record one request's cost. Zero/negative/non-finite costs are ignored (a
    /// $0 cached request consumes no budget). Updates memory, then persists.
    pub async fn record(&self, provider: &str, cost: f64, now: i64) {
        if !cost.is_finite() || cost <= 0.0 {
            return;
        }
        {
            let mut map = self.events.lock().unwrap_or_else(|e| e.into_inner());
            let v = map.entry(provider.to_string()).or_default();
            v.push((now, cost));
            let cutoff = now - RETAIN_SECS;
            v.retain(|(ts, _)| *ts >= cutoff);
        }
        if let Some(pool) = &self.pool
            && let Err(e) = store::insert_usage_event(pool, provider, now, cost).await
        {
            tracing::warn!(provider, "failed to persist usage event: {e}");
        }
    }

    /// Per-window spend/remaining for a provider at time `now`.
    pub fn windows(&self, provider: &str, windows: &[CostWindow], now: i64) -> Vec<WindowStat> {
        let map = self.events.lock().unwrap_or_else(|e| e.into_inner());
        let events = map.get(provider);
        windows
            .iter()
            .map(|w| {
                let cutoff = now - w.window_secs as i64;
                let mut spent = 0.0f64;
                let mut oldest: Option<i64> = None;
                if let Some(evts) = events {
                    for (ts, cost) in evts {
                        if *ts > cutoff {
                            spent += *cost;
                            if oldest.is_none() {
                                oldest = Some(*ts); // ascending -> first in-window is oldest
                            }
                        }
                    }
                }
                let remaining = (w.limit - spent).max(0.0);
                let reset_in_secs = match oldest {
                    Some(ts) => (ts + w.window_secs as i64 - now).max(0),
                    None => w.window_secs as i64,
                };
                WindowStat {
                    label: w.label.clone(),
                    limit: w.limit,
                    spent,
                    remaining,
                    reset_in_secs,
                }
            })
            .collect()
    }

    /// Whether any configured window is exhausted (`remaining <= 0`) — for failover.
    pub fn exhausted(&self, provider: &str, windows: &[CostWindow], now: i64) -> bool {
        !windows.is_empty()
            && self
                .windows(provider, windows, now)
                .iter()
                .any(|w| w.remaining <= 0.0)
    }

    /// The set of providers with at least one exhausted window — passed to the
    /// router so failover proactively skips them (before they 429).
    pub fn exhausted_set(
        &self,
        providers: &HashMap<String, Provider>,
        now: i64,
    ) -> HashSet<String> {
        providers
            .iter()
            .filter(|(name, p)| self.exhausted(name, &p.cost_windows, now))
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// Parse a `cost` JSON value — Zen sends a string (`"0.0023"`); some providers a
/// number. Returns `None` for anything else (so an absent cost is simply skipped).
pub fn parse_cost(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Extract `(prompt_tokens, completion_tokens)` from a response's `usage` object.
pub fn tokens_from_usage(usage: Option<&Value>) -> (Option<u64>, Option<u64>) {
    match usage {
        Some(u) => (
            u.get("prompt_tokens").and_then(Value::as_u64),
            u.get("completion_tokens").and_then(Value::as_u64),
        ),
        None => (None, None),
    }
}

/// The cost to charge against the windows: the upstream's real `cost` when it's
/// non-zero, otherwise a token×price estimate (subscription plans report $0).
/// Returns `None` when neither a real cost nor a price is available.
pub fn effective_cost(
    real: Option<f64>,
    price: Option<&ModelPrice>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
) -> Option<f64> {
    if let Some(c) = real
        && c > 0.0
    {
        return Some(c);
    }
    price.map(|p| p.estimate(prompt_tokens.unwrap_or(0), completion_tokens.unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(label: &str, secs: u64, limit: f64) -> CostWindow {
        CostWindow {
            label: label.into(),
            window_secs: secs,
            limit,
        }
    }

    #[tokio::test]
    async fn windows_sum_within_horizon() {
        let m = UsageMeter::new(None);
        let now = 1_000_000i64;
        m.record("go", 3.0, now - 100).await; // in 5h
        m.record("go", 4.0, now - 10_000).await; // in 5h
        m.record("go", 5.0, now - 20_000).await; // outside 5h (18000), inside 7d
        let win = vec![w("5h", 18000, 12.0), w("7d", 604800, 30.0)];
        let stats = m.windows("go", &win, now);
        assert_eq!(stats[0].label, "5h");
        assert_eq!(stats[0].spent, 7.0);
        assert_eq!(stats[0].remaining, 5.0);
        assert_eq!(stats[1].spent, 12.0);
        assert_eq!(stats[1].remaining, 18.0);
    }

    #[tokio::test]
    async fn exhausted_when_over_limit() {
        let m = UsageMeter::new(None);
        let now = 1_000i64;
        let win = vec![w("5h", 18000, 12.0)];
        assert!(!m.exhausted("go", &win, now));
        m.record("go", 12.5, now).await;
        assert!(m.exhausted("go", &win, now));
        assert!(!m.exhausted("zen", &win, now)); // unknown provider
        assert!(!m.exhausted("go", &[], now)); // no windows configured
    }

    #[tokio::test]
    async fn zero_cost_is_ignored() {
        let m = UsageMeter::new(None);
        m.record("go", 0.0, 100).await;
        m.record("go", -1.0, 100).await;
        let stats = m.windows("go", &[w("5h", 18000, 12.0)], 100);
        assert_eq!(stats[0].spent, 0.0);
    }

    #[tokio::test]
    async fn reset_in_secs_tracks_oldest_in_window() {
        let m = UsageMeter::new(None);
        let now = 100_000i64;
        m.record("go", 1.0, now - 1000).await; // oldest
        m.record("go", 1.0, now - 500).await;
        let stats = m.windows("go", &[w("5h", 18000, 12.0)], now);
        assert_eq!(stats[0].reset_in_secs, 17000); // (now-1000)+18000 - now
    }

    #[tokio::test]
    async fn load_seeds_events() {
        let m = UsageMeter::new(None);
        m.load(vec![("go".into(), 100, 2.0), ("go".into(), 200, 3.0)]);
        let stats = m.windows("go", &[w("5h", 18000, 12.0)], 210);
        assert_eq!(stats[0].spent, 5.0);
    }

    #[test]
    fn parse_cost_handles_string_and_number() {
        assert_eq!(parse_cost(&serde_json::json!("1.5")), Some(1.5));
        assert_eq!(parse_cost(&serde_json::json!(2.0)), Some(2.0));
        assert_eq!(parse_cost(&serde_json::json!("0")), Some(0.0));
        assert_eq!(parse_cost(&serde_json::json!("abc")), None);
        assert_eq!(parse_cost(&serde_json::json!(null)), None);
    }

    #[test]
    fn model_price_estimate() {
        let p = ModelPrice {
            input: 0.4,
            output: 1.6,
        };
        assert_eq!(p.estimate(1_000_000, 1_000_000), 2.0);
        assert!((p.estimate(100, 200) - 0.00036).abs() < 1e-9);
        assert_eq!(p.estimate(0, 0), 0.0);
    }

    #[test]
    fn effective_cost_prefers_real_then_estimate() {
        let p = ModelPrice {
            input: 0.4,
            output: 1.6,
        };
        // real non-zero wins, price ignored
        assert_eq!(
            effective_cost(Some(0.5), Some(&p), Some(999), Some(999)),
            Some(0.5)
        );
        // real $0 (subscription) -> estimate from tokens
        assert_eq!(
            effective_cost(Some(0.0), Some(&p), Some(1_000_000), Some(0)),
            Some(0.4)
        );
        // real missing -> estimate
        assert_eq!(
            effective_cost(None, Some(&p), Some(0), Some(1_000_000)),
            Some(1.6)
        );
        // no price + $0/None real -> nothing to record
        assert_eq!(effective_cost(Some(0.0), None, Some(10), Some(10)), None);
        assert_eq!(effective_cost(None, None, None, None), None);
    }

    #[test]
    fn tokens_from_usage_extracts() {
        let u = serde_json::json!({ "prompt_tokens": 92, "completion_tokens": 200, "total_tokens": 292 });
        assert_eq!(tokens_from_usage(Some(&u)), (Some(92), Some(200)));
        assert_eq!(tokens_from_usage(None), (None, None));
        assert_eq!(
            tokens_from_usage(Some(&serde_json::json!({}))),
            (None, None)
        );
    }
}
