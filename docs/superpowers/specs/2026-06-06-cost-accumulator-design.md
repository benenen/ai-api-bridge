# Cost Accumulator — Design

Date: 2026-06-06
Status: approved — implementing

## Goal

Track per-provider spend in rolling cost windows so the bridge can show "remaining"
per window and fail over **before** a provider's subscription window is exhausted.

Motivating case: OpenCode Zen's **Go** plan limits spend per rolling window — **$12 /
5h, $30 / weekly, $60 / monthly** — but exposes **no usage API or rate-limit header**
to the inference key (confirmed by probing + docs + open request anomalyco/opencode#10448).
The only API-visible signal is the per-request **`cost`** field on each completion. So
the bridge self-accumulates that `cost` into rolling windows.

## Confirmed decisions

1. **Passthrough `/v1/chat/completions` streaming = accepted gap.** It forwards bytes
   verbatim; we do *not* parse cost there. Codex→`/v1/responses` and Claude Code→
   `/v1/messages` (and all non-stream paths) are fully covered.
2. **Feed windows to failover.** A provider with any window `remaining ≤ 0` is treated
   as degraded by `is_usable` — proactively skipped, complementing the reactive 429.
3. **Streaming cost = best-effort.** Captured from the `{"cost":…}` chunk that Zen sends
   **after `[DONE]`**; if the client disconnects the instant it sees the terminal event
   we may miss it (it arrives in the same burst, so usually captured). No per-model price
   table.

## Where `cost` comes from (verified)

- **Non-stream**: top-level `cost` field on the response JSON. Value is a **string**
  (`"0.0023"`) or number — parse string-or-number → `f64`.
- **Stream**: a separate SSE event **after `[DONE]`**: `data: {"choices":[],"cost":"0"}`.
  `run_stream` currently stops at `[DONE]`; it must drain past it to read this chunk.

## Components

### `src/usage.rs` — `UsageMeter`
Mirrors the `StatusMap` pattern: in-memory authoritative, SQLite for durability.

- State: `Mutex<HashMap<String, VecDeque<(i64 ts_secs, f64 cost)>>>` + `Option<SqlitePool>`.
- `record(provider, cost, now)` *(async)*: push `(now,cost)`, evict events older than the
  retention horizon (31 days, ≥ the largest standard window), then INSERT one row.
- `windows(provider, &[CostWindow], now) -> Vec<WindowStat>`: per window, `spent` = Σ cost
  where `ts > now − window_secs`; `remaining = max(0, limit − spent)`; `reset_in_secs` =
  `(oldest in-window ts + window_secs) − now` (soonest partial relief), else `window_secs`.
- `exhausted(provider, &[CostWindow], now) -> bool`: any window `remaining ≤ 0`.
- `parse_cost(&Value) -> Option<f64>`: handles `String` and `Number`; `None` otherwise.
- Startup: `load(events_since)` seeds the deques from SQLite.

`WindowStat { label, limit, spent, remaining, reset_in_secs }` (Serialize).

### `config.rs`
```rust
pub struct CostWindow { pub label: String, pub window_secs: u64, pub limit: f64 }
// Provider gains:  #[serde(default)] pub cost_windows: Vec<CostWindow>
```
Empty list ⇒ provider is not cost-tracked. `go` default: `5h`=18000s/$12, `7d`=604800s/$30,
`30d`=2592000s/$60 (added to `bridge.toml` + `bridge.example.toml`).

### Store / migration (`migrations/0003_usage.sql`)
```sql
ALTER TABLE providers ADD COLUMN cost_windows TEXT NOT NULL DEFAULT '[]';
CREATE TABLE usage_events (provider TEXT NOT NULL, ts INTEGER NOT NULL, cost REAL NOT NULL);
CREATE INDEX idx_usage_events_provider_ts ON usage_events(provider, ts);
```
`store.rs`: `cost_windows` JSON in seed/load/get/insert/update provider (like `extra_headers`);
`insert_usage_event`, `load_usage_events(since)`, `prune_usage_events(before)`.

### `server.rs`
- `call_upstream_json` → `(String served_provider, Value)`; `open_upstream_stream` →
  `(String served_provider, ByteStream)`.
- `AppState` gains `usage: Arc<UsageMeter>`.
- Non-stream handlers (`responses`/`messages`/`chat`): after the call, `parse_cost(json["cost"])`
  → `usage.record(provider, c, now)`.
- Stream handlers (`responses`/`messages`): pass a `CostRecorder { meter, provider }` into
  `run_stream`, which scans every data chunk for a `cost` field and, on `[DONE]`, keeps
  draining the upstream (without yielding client frames) to catch the trailing cost chunk,
  then records.
- Passthrough `/v1/chat/completions` stream: no capture (decision 1).

### Failover (`router.rs`)
`resolve_candidates(cfg, status, exhausted: &HashSet<String>, alias)`; `is_usable` returns
false when the provider is in `exhausted`. Handlers build the set via
`usage.exhausted_set(&cfg, now)` (the same in-memory read used by status).

### Exposure
`/v1/providers` + `/admin/api/providers` add `cost_windows: [{label,limit,spent,remaining,
reset_in_secs}]` per provider. Admin dashboard renders a small spent/limit bar per window
under each provider card; the provider edit form edits `cost_windows` (JSON).

## ⑤ Providers without cost
- No `cost_windows` configured → not tracked (no windows, no failover effect).
- Configured but provider never returns `cost` → `spent` stays 0, `remaining = limit`
  (always usable; no false exhaustion). Token-price estimation is out of scope.

## Testing
- **usage.rs (TDD)**: window sums at 5h/7d/30d boundaries, eviction, remaining/exhausted,
  `reset_in_secs`, `parse_cost` (string/number/missing) — all with injected `now`.
- **store.rs**: `cost_windows` provider roundtrip; `usage_events` insert/load/prune.
- **router.rs**: an exhausted provider is ordered last / excluded (extend existing tests
  with an `exhausted` set).
- **Smoke (real curl)**: start the bridge with `go` windows; a non-stream completion and a
  streaming completion against Zen each bump `spent`; `/v1/providers` shows
  `spent>0, remaining<limit`. (Runs with proxy unset for direct egress.)

## Files
new `src/usage.rs`, `migrations/0003_usage.sql`; edit `config.rs`, `store.rs`, `server.rs`,
`router.rs`, `admin.rs`, `web/admin.html`, `main.rs`, `lib.rs`, `bridge.toml`,
`bridge.example.toml`, `tests/pipeline.rs` (AppState shape).
