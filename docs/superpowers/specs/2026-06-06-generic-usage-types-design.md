# Generic Usage Types — Design

Date: 2026-06-06
Status: approved — implementing

## Goal

Generalize the cost-specific usage model into a **typed** one so a provider can be
metered by different "currencies": **billing ($)**, **count (requests)**, **token**,
… — each as rolling limit windows. Render per-provider usage by type in the admin
page. Stay backward-compatible with the existing `cost_windows` config.

## Key insight

The rolling-window math is already type-agnostic: `spent = Σ amount in window`,
`remaining = limit − spent`, `exhausted = remaining ≤ 0`. A usage *type* only changes
(a) how the per-request `amount` is derived and (b) the display unit/format. So we add a
`usage_type` dimension; the `UsageMeter` machinery is reused.

## Confirmed decisions

1. Ship **billing + count + token** (enum extensible).
2. `Provider.usage: Vec<UsageSpec>` — a provider may have multiple typed specs.
3. Keep legacy `cost_windows`/`model_prices` and **fold them into a Billing spec at
   load** (back-compatible, no data-migration SQL).
4. Rename `usage_events.cost` → `amount`.

## Data model (`config.rs`)

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageKind { Billing, Count, Token }
// unit(): billing→"usd", count→"requests", token→"tokens"
// as_str(): "billing"/"count"/"token"; parse(&str)->Option<UsageKind>

pub struct UsageWindow { label: String, window_secs: u64, limit: f64 } // limit in the kind's unit

#[derive(Serialize, Deserialize)]
#[serde(tag = "usage_type", rename_all = "snake_case")]
pub enum UsageSpec {
    Billing { #[serde(default)] windows: Vec<UsageWindow>,
              #[serde(default)] model_prices: HashMap<String, ModelPrice> },
    Count   { #[serde(default)] windows: Vec<UsageWindow> },
    Token   { #[serde(default)] windows: Vec<UsageWindow> },
}
// kind(), windows() -> &[UsageWindow], model_prices() -> Option<&HashMap<..>>
```

`Provider` gains `#[serde(default)] pub usage: Vec<UsageSpec>`. The serde shape is exactly
`{usage_type, …data}`, e.g.
`{"usage_type":"billing","windows":[{"label":"5h","window_secs":18000,"limit":12}],"model_prices":{…}}`.

Legacy `cost_windows: Vec<CostWindow>` + `model_prices: HashMap<String,ModelPrice>` stay
as `#[serde(default)]` **deprecated input**. `CostWindow`/`ModelPrice` structs are kept.

### Compat fold

`Provider::normalize_usage(&mut self)`: if `usage` is empty and `cost_windows` or
`model_prices` is non-empty, set `usage = [Billing { windows: <cost_windows mapped to
UsageWindow>, model_prices }]`. Called after parsing — in `store::row_to_provider` (DB
load) and for each provider in `Config::from_toml` (TOML seed). Runtime then always reads
`provider.usage`; the legacy fields are ignored once `usage` is populated. New admin
writes set `usage` directly.

## Migration `0005_usage_types.sql`

```sql
ALTER TABLE providers ADD COLUMN usage TEXT NOT NULL DEFAULT '[]';
ALTER TABLE usage_events RENAME COLUMN cost TO amount;
ALTER TABLE usage_events ADD COLUMN usage_type TEXT NOT NULL DEFAULT 'billing';
```
Existing `usage_events` rows become `(usage_type='billing', amount=<old cost>)`. The
`(provider, ts)` index is unchanged. Existing provider rows keep `cost_windows`/
`model_prices`; `usage` defaults `'[]'` and is folded at load.

## Store (`store.rs`)

- `PROVIDER_COLS` += `usage`; `ProviderRow` += `usage: String`; `row_to_provider` parses
  `usage` then calls `normalize_usage`. seed/insert/update bind `serde_json::to_string(&p.usage)`.
  Legacy `cost_windows`/`model_prices` columns still written (compat) — admin writes set
  `usage` and clears legacy.
- Events: `insert_usage_event(pool, provider, ts, kind: &str, amount)`;
  `load_usage_events(since) -> Vec<(String, i64, String, f64)>` (provider, ts, usage_type,
  amount); `prune_usage_events(before)` unchanged.

## UsageMeter (`usage.rs`)

- Events keyed by kind: `Mutex<HashMap<String, HashMap<UsageKind, Vec<(i64, f64)>>>>`.
- `record(provider, kind, amount, now)` *(async)* — push + evict (RETAIN_SECS=31d) + persist.
- `windows(provider, kind, &[UsageWindow], now) -> Vec<WindowStat>` — sums that kind's events.
- `exhausted(provider, specs: &[UsageSpec], now) -> bool` — any window of any spec ≤ 0.
- `exhausted_set(providers, now) -> HashSet<String>` — uses `provider.usage`.
- `load(Vec<(provider, ts, kind, amount)>)`.
- Helpers retained: `parse_cost`, `tokens_from_usage`, `effective_cost`. New:
  `amount_for(kind, price: Option<&ModelPrice>, real, pt, ct) -> Option<f64>`:
  billing→`effective_cost`, count→`Some(1.0)`, token→`Some((pt+ct) as f64)`.
- `WindowStat { label, limit, spent, remaining, reset_in_secs }` unchanged.

## Record path (`server.rs`)

`record_usage(state, provider, model, resp)` (and the streaming equivalent):
1. Gate on `usage_enabled()` (master switch unchanged; 429 failover independent).
2. Compute signals once: `real = parse_cost(resp.cost)`, `(pt, ct) = tokens_from_usage`.
3. For each `spec` in `provider.usage`: `price = spec.model_prices()?.get(model)` (billing
   only); `amount = amount_for(spec.kind(), price, real, pt, ct)`; `record(provider,
   spec.kind(), amount, now)` (skips ≤0). A provider with no specs records nothing.

Streaming: `CostRecorder` holds `provider` + a precomputed `Vec<(UsageKind, Option<ModelPrice>)>`
(billing carries the served model's price). `run_stream` captures real cost + tokens (as
now, gated by `recorder.is_some()`), then at stream end records each kind. `open_upstream_stream`/
`call_upstream_json` already return the served `(provider, upstream_model)`.

## Exposure + render

`/v1/providers` + `/admin/api/providers`, per provider, emit (when tracking on):
```json
"usage":[
  {"usage_type":"billing","unit":"usd","model_prices":{…},
   "windows":[{"label":"5h","window_secs":18000,"limit":12,"spent":3.2,"remaining":8.8,"reset_in_secs":…}]},
  {"usage_type":"count","unit":"requests",
   "windows":[{"label":"1d","window_secs":86400,"limit":500,"spent":180,"remaining":320,"reset_in_secs":…}]}
]
```
When off: windows are config-only (`label/window_secs/limit`, no spend), plus top-level
`cost_tracking` flag (unchanged gating). The admin form edits this `usage` JSON (one
textarea, replacing the separate cost_windows + model_prices fields).

Admin page render: one section per usage group, header `BILLING (usd)` / `COUNT (requests)`
/ `TOKEN (tokens)`, each window a shared remaining-bar; number formatting by unit —
billing `$3.20 / $12`, count `180 / 500 reqs`, token `42k / 200k tok`.

## Failover

`exhausted_set` iterates each provider's `usage` specs × windows; `remaining ≤ 0` in any
window (any unit) demotes the provider — same generic rule, just multi-kind. Reactive 429
failover stays independent (`is_retryable`).

## Testing

- `usage.rs`: record/windows per kind; `exhausted` across mixed kinds; `amount_for` per
  kind (billing via effective_cost, count=1, token=pt+ct).
- `config.rs`/`store.rs`: `UsageSpec` JSON roundtrip; **compat fold** (a provider with only
  `cost_windows` loads as one Billing spec); `usage` column roundtrip.
- `tests/pipeline.rs`: update `usage_toggle_gates_recording` (provider now carries a Billing
  spec via folded `cost_windows`; `windows(provider, Billing, …)`); add a count-type test
  (provider with a `count` spec → a request increments the count window).
- Real curl smoke: a `go` provider with billing (existing) + a `count` window — a request
  bumps both `$` spend (estimate) and request count at `/v1/providers`.

## Files

new `migrations/0005_usage_types.sql`; edit `config.rs`, `store.rs`, `usage.rs`,
`server.rs`, `admin.rs`, `web/admin.html`, `tests/pipeline.rs`, `bridge.example.toml`,
`CLAUDE.md`. (`bridge.toml` is gitignored — update locally for the smoke.)

## Back-compat summary

Existing DBs + `bridge.toml` with `cost_windows`/`model_prices` keep working unchanged
(folded to a Billing spec at load). `usage_events` rows are renamed in place
(`cost→amount`, `usage_type='billing'`). No data-migration SQL beyond the schema ALTERs.
