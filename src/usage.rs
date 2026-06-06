//! Usage accumulator: per-provider rolling limit windows, by usage *kind*.
//!
//! A provider can be metered in several "currencies" — billing ($), request count,
//! tokens — each as rolling windows. Events carry a [`UsageKind`] + an `amount` in
//! that kind's unit; the window math (`spent = Σ amount`, `remaining = limit −
//! spent`, `exhausted = remaining ≤ 0`) is identical across kinds. The in-memory
//! log is authoritative for reads (status + failover); SQLite persists it.
//!
//! Some providers (e.g. OpenCode Zen Go) report `cost = $0` (subscription), so for
//! billing the amount falls back to a token × price estimate (see [`amount_for`]).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde_json::Value;
use sqlx::sqlite::SqlitePool;

use crate::config::{ModelPrice, Provider, UsageKind, UsageSpec, UsageWindow};
use crate::store;

/// Retention horizon: events older than this are dropped (≥ the largest standard
/// window of 30d, with a day of slack).
const RETAIN_SECS: i64 = 31 * 24 * 3600;

/// Per-window usage snapshot (exposed at `/v1/providers` + the admin page).
#[derive(Debug, Clone, serde::Serialize)]
pub struct WindowStat {
    pub label: String,
    pub limit: f64,
    pub spent: f64,
    pub remaining: f64,
    /// Seconds until the oldest in-window event ages out (soonest partial relief).
    pub reset_in_secs: i64,
}

/// Per-kind event log for one provider: kind -> (ts_secs, amount), ascending by ts.
type ProviderEvents = HashMap<UsageKind, Vec<(i64, f64)>>;

/// In-memory rolling usage log per (provider, kind), persisted to SQLite.
pub struct UsageMeter {
    events: Mutex<HashMap<String, ProviderEvents>>,
    pool: Option<SqlitePool>,
}

impl UsageMeter {
    pub fn new(pool: Option<SqlitePool>) -> Self {
        Self {
            events: Mutex::new(HashMap::new()),
            pool,
        }
    }

    /// Seed the in-memory log from persisted events `(provider, ts, usage_type,
    /// amount)` (call once at startup). Unknown kinds are skipped.
    pub fn load(&self, events: Vec<(String, i64, String, f64)>) {
        let mut map = self.events.lock().unwrap_or_else(|e| e.into_inner());
        for (provider, ts, kind, amount) in events {
            let Some(kind) = UsageKind::parse(&kind) else {
                continue;
            };
            map.entry(provider)
                .or_default()
                .entry(kind)
                .or_default()
                .push((ts, amount));
        }
        for km in map.values_mut() {
            for v in km.values_mut() {
                v.sort_by_key(|(ts, _)| *ts);
            }
        }
    }

    /// Record one request's usage `amount` for a kind. Zero/negative/non-finite
    /// amounts are ignored. Updates memory, then persists.
    pub async fn record(&self, provider: &str, kind: UsageKind, amount: f64, now: i64) {
        if !amount.is_finite() || amount <= 0.0 {
            return;
        }
        {
            let mut map = self.events.lock().unwrap_or_else(|e| e.into_inner());
            let v = map
                .entry(provider.to_string())
                .or_default()
                .entry(kind)
                .or_default();
            v.push((now, amount));
            let cutoff = now - RETAIN_SECS;
            v.retain(|(ts, _)| *ts >= cutoff);
        }
        if let Some(pool) = &self.pool
            && let Err(e) =
                store::insert_usage_event(pool, provider, now, kind.as_str(), amount).await
        {
            tracing::warn!(provider, "failed to persist usage event: {e}");
        }
    }

    /// Per-window spent/remaining for a provider's kind at time `now`.
    pub fn windows(
        &self,
        provider: &str,
        kind: UsageKind,
        windows: &[UsageWindow],
        now: i64,
    ) -> Vec<WindowStat> {
        let map = self.events.lock().unwrap_or_else(|e| e.into_inner());
        let events = map.get(provider).and_then(|km| km.get(&kind));
        windows
            .iter()
            .map(|w| {
                let cutoff = now - w.window_secs as i64;
                let mut spent = 0.0f64;
                let mut oldest: Option<i64> = None;
                if let Some(evts) = events {
                    for (ts, amount) in evts {
                        if *ts > cutoff {
                            spent += *amount;
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

    /// Whether any window of any of the provider's usage specs is exhausted
    /// (`remaining <= 0`, in any unit) — for failover.
    pub fn exhausted(&self, provider: &str, specs: &[UsageSpec], now: i64) -> bool {
        specs.iter().any(|spec| {
            self.windows(provider, spec.kind(), spec.windows(), now)
                .iter()
                .any(|w| w.remaining <= 0.0)
        })
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
            .filter(|(name, p)| self.exhausted(name, &p.usage, now))
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// The amount to charge a kind for one request: billing → real cost or token×price
/// estimate; count → 1; token → prompt + completion tokens.
pub fn amount_for(
    kind: UsageKind,
    price: Option<&ModelPrice>,
    real: Option<f64>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
) -> Option<f64> {
    match kind {
        UsageKind::Billing => effective_cost(real, price, prompt_tokens, completion_tokens),
        UsageKind::Count => Some(1.0),
        UsageKind::Token => {
            Some((prompt_tokens.unwrap_or(0) + completion_tokens.unwrap_or(0)) as f64)
        }
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

    use UsageKind::{Billing, Count, Token};

    fn w(label: &str, secs: u64, limit: f64) -> UsageWindow {
        UsageWindow {
            label: label.into(),
            window_secs: secs,
            limit,
        }
    }
    fn billing(windows: Vec<UsageWindow>) -> UsageSpec {
        UsageSpec::Billing {
            windows,
            model_prices: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn windows_sum_within_horizon() {
        let m = UsageMeter::new(None);
        let now = 1_000_000i64;
        m.record("go", Billing, 3.0, now - 100).await; // in 5h
        m.record("go", Billing, 4.0, now - 10_000).await; // in 5h
        m.record("go", Billing, 5.0, now - 20_000).await; // outside 5h, inside 7d
        let win = vec![w("5h", 18000, 12.0), w("7d", 604800, 30.0)];
        let stats = m.windows("go", Billing, &win, now);
        assert_eq!(stats[0].label, "5h");
        assert_eq!(stats[0].spent, 7.0);
        assert_eq!(stats[0].remaining, 5.0);
        assert_eq!(stats[1].spent, 12.0);
        assert_eq!(stats[1].remaining, 18.0);
    }

    #[tokio::test]
    async fn kinds_are_independent() {
        let m = UsageMeter::new(None);
        let now = 1_000i64;
        m.record("go", Billing, 2.0, now).await;
        m.record("go", Count, 1.0, now).await;
        m.record("go", Count, 1.0, now).await;
        m.record("go", Token, 500.0, now).await;
        let win = vec![w("5h", 18000, 9999.0)];
        assert_eq!(m.windows("go", Billing, &win, now)[0].spent, 2.0);
        assert_eq!(m.windows("go", Count, &win, now)[0].spent, 2.0);
        assert_eq!(m.windows("go", Token, &win, now)[0].spent, 500.0);
    }

    #[tokio::test]
    async fn exhausted_across_mixed_kinds() {
        let m = UsageMeter::new(None);
        let now = 1_000i64;
        let specs = vec![
            billing(vec![w("5h", 18000, 10.0)]),
            UsageSpec::Count {
                windows: vec![w("1d", 86400, 3.0)],
            },
        ];
        assert!(!m.exhausted("go", &specs, now));
        for _ in 0..3 {
            m.record("go", Count, 1.0, now).await; // fill the count window only
        }
        assert!(m.exhausted("go", &specs, now));
        assert!(!m.exhausted("go", &[], now)); // no specs
    }

    #[tokio::test]
    async fn zero_amount_is_ignored() {
        let m = UsageMeter::new(None);
        m.record("go", Billing, 0.0, 100).await;
        m.record("go", Billing, -1.0, 100).await;
        assert_eq!(
            m.windows("go", Billing, &[w("5h", 18000, 12.0)], 100)[0].spent,
            0.0
        );
    }

    #[tokio::test]
    async fn reset_in_secs_tracks_oldest_in_window() {
        let m = UsageMeter::new(None);
        let now = 100_000i64;
        m.record("go", Billing, 1.0, now - 1000).await; // oldest
        m.record("go", Billing, 1.0, now - 500).await;
        let stats = m.windows("go", Billing, &[w("5h", 18000, 12.0)], now);
        assert_eq!(stats[0].reset_in_secs, 17000); // (now-1000)+18000 - now
    }

    #[tokio::test]
    async fn load_seeds_events() {
        let m = UsageMeter::new(None);
        m.load(vec![
            ("go".into(), 100, "billing".into(), 2.0),
            ("go".into(), 200, "billing".into(), 3.0),
            ("go".into(), 200, "count".into(), 1.0),
        ]);
        assert_eq!(
            m.windows("go", Billing, &[w("5h", 18000, 12.0)], 210)[0].spent,
            5.0
        );
        assert_eq!(
            m.windows("go", Count, &[w("5h", 18000, 12.0)], 210)[0].spent,
            1.0
        );
    }

    #[test]
    fn amount_for_by_kind() {
        let p = ModelPrice {
            input: 0.4,
            output: 1.6,
        };
        // billing: $0 real -> token estimate; real>0 wins
        assert_eq!(
            amount_for(Billing, Some(&p), Some(0.0), Some(1_000_000), Some(0)),
            Some(0.4)
        );
        assert_eq!(
            amount_for(Billing, Some(&p), Some(0.5), Some(9), Some(9)),
            Some(0.5)
        );
        // count: always 1; token: prompt + completion
        assert_eq!(
            amount_for(Count, None, Some(0.0), Some(9), Some(9)),
            Some(1.0)
        );
        assert_eq!(
            amount_for(Token, None, None, Some(92), Some(200)),
            Some(292.0)
        );
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
