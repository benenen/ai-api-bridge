# Provider & Route Admin UI — Design

Date: 2026-06-06
Status: approved (brainstorming) — pending implementation review

## Goal

A built-in web page to CRUD-manage **providers** and **routes** that currently live
in SQLite (`store.rs`, seeded once from `bridge.toml`). Today the only way to change
them is editing `bridge.toml` + `--reseed`, which wipes the DB. This adds a live admin
surface: list / create / edit / delete, with changes taking effect at runtime without
a restart.

Out of scope: editing `provider_status` (watcher-owned), `default_provider`/`auth_token`
(process-level config), and authentication beyond reusing the existing `auth_token`.

## The core constraint

The DB is loaded into an **immutable** in-memory `Config` once at startup
(`main.rs`), and `watcher::spawn` forks one probe task per provider **at startup** and
never re-reads. So the hard part of this feature is not the CRUD — it is making a DB
write propagate to two live consumers:

1. the **router** (reads `state.config.providers` / `.routes` per request), and
2. the **watcher** (one task per provider, holding a `Provider` snapshot from startup).

`router::Resolved<'a>` borrows `&'a Provider` out of `Config`, and handlers hold those
candidates across `.await`. A std lock guard cannot be held across an await in a `Send`
future, so the config cannot simply live behind a `RwLock<Config>` that handlers read
through. The fix is a **snapshot swap**.

## Architecture

### 1. Mutable config via snapshot swap

`AppState.config: Config` → `AppState.config: RwLock<Arc<Config>>`.

- Handlers take a cheap snapshot at entry: `let cfg = state.config.read().unwrap().clone();`
  (clones an `Arc`, drops the guard immediately) and use `&cfg` for the whole request.
  `Resolved<'a>` now borrows from the owned `Arc<Config>`, which lives for the request —
  no guard held across `.await`.
- In-flight requests keep their old snapshot; a concurrent admin write swaps the `Arc`
  for *subsequent* requests only. No locking on the hot path beyond a momentary read.
- This mirrors the existing `StatusMap = Arc<RwLock<HashMap<..>>>` pattern, so **no new
  dependency** (no `arc-swap`).

Ripple: ~6 handlers (`responses_handler`, `chat_handler`, `messages_handler`,
`providers_status`, `list_models`, and `check_auth`) gain a one-line snapshot. `check_auth`
takes `&Config` instead of `&AppState`.

### 2. Watcher reconcile

- `watcher::spawn(...)` returns `Vec<JoinHandle<()>>`.
- `AppState` gains `watchers: std::sync::Mutex<Vec<JoinHandle<()>>>`.
- New `watcher::reconcile(pool, providers, status, handles)`: aborts the current handles
  and respawns from the new provider set. CRUD is rare (a human clicking buttons), so
  abort-all + respawn is simpler than surgical per-provider task tracking and is correct
  for add / edit (probe settings) / delete alike.

### 3. The reload step (the answer to "how does state sync")

Every successful provider **or** route write runs `server::reload_from_db(&state)`:

1. Rebuild a fresh `Config` from the DB (`store::load_into_config` into a clone of the
   current snapshot to preserve `listen`/`database`/`default_provider`/`auth_token`),
   then `apply_env_overrides()`.
2. Swap `AppState.config`'s `Arc`.
3. `watcher::reconcile(...)` — abort + respawn probe tasks from the new provider set.
4. Prune the `StatusMap`: drop entries for providers no longer present (a deleted
   provider's `provider_status` row is already removed by `ON DELETE CASCADE`).

Route writes do steps 1–2 (and 4 is a no-op); they don't change the provider set so the
watcher restart is harmless but skipped when only routes changed. (Implementation: a
single `reload_from_db` that always rebuilds + reconciles is simplest and correct;
reconcile is cheap.)

## CRUD API

Dedicated `/admin` namespace, kept off the `/v1/*` paths Codex/Claude Code point at.
All endpoints reuse `check_auth` (so they honor `auth_token` when set, open otherwise).
Errors use the existing `BridgeError` → JSON shape.

### Providers

- `GET /admin/api/providers` — list. Each item: full editable config **+ merged live
  watcher status**, with `api_key` **masked** (returns `api_key_set: bool`, never the
  secret). One endpoint powers the whole providers table + its status pills.
- `POST /admin/api/providers` — create. Body = provider config (incl. plaintext
  `api_key`). Validates: `name` non-empty + unique, `wire` ∈ {openai-chat,
  openai-responses, anthropic-messages}, `base_url` non-empty. 409 on duplicate name.
- `PUT /admin/api/providers/:name` — update. `name` is the immutable PK (rename =
  delete+create). **Blank/omitted `api_key` preserves the stored key**; a non-empty
  value replaces it. 404 if absent.
- `DELETE /admin/api/providers/:name` — delete. `ON DELETE CASCADE` removes its routes +
  status row. Response notes how many routes were cascaded so the UI can warn.

### Routes

- `GET /admin/api/routes` — list (`alias`, `provider`, `model`, `fallback[]`).
- `POST /admin/api/routes` — create. Validates `alias` non-empty + unique, `provider`
  exists, `model` non-empty. `fallback` is an array of `{provider, model}`.
- `PUT /admin/api/routes/:alias` — update (alias = immutable PK). 404 if absent.
- `DELETE /admin/api/routes/:alias` — delete.

### Request/response shapes

Input DTOs are dedicated structs (not `Provider`/`Route` directly) so we control
`api_key`-preserve semantics and validation. `extra_headers` is a JSON object;
`fallback` a JSON array of `{provider, model}`.

## Store layer (`store.rs`)

New functions, mirroring the existing seed/load style (sqlx runtime queries):

- `insert_provider(pool, name, &Provider)` / `update_provider(pool, name, &Provider)` /
  `delete_provider(pool, name)`
- `get_provider(pool, name) -> Option<Provider>` (to read the stored `api_key` for the
  preserve-on-blank update path)
- `insert_route(pool, &Route)` / `update_route(pool, alias, &Route)` /
  `delete_route(pool, alias)`

`update_*` returns the affected row count so handlers can distinguish 404 from success.

## Serving the page

`include_str!("../web/admin.html")` embedded into the binary, served as
`Html(..)` at `GET /` and `GET /admin`. A single self-contained file → no new crate
(`rust-embed`/`ServeDir` would break the single static-musl-binary the release workflow
ships). The page (and the rest of the binary) keeps working offline.

## Auth

Reuse `auth_token` / `check_auth` unchanged (accepts `Authorization: Bearer <token>` or
`x-api-key: <token>`). The page:

- sends the stored token as `Authorization: Bearer` on every admin fetch;
- on a `401`, prompts for the token and stores it in `localStorage`;
- when `auth_token` is unset server-side, the API is open (consistent with current
  behavior + the default localhost bind).

The `GET /` page itself is served without auth (it's a static shell); the data behind it
is gated by the admin API.

## Frontend

`web/admin.html` — one self-contained file, **vanilla JS `fetch` + hand-written CSS, no
runtime CDN**. Dark technical dashboard:

- dark slate palette, monospace accents;
- providers table + routes table;
- status pills green / amber / red (available / degraded-or-low-quota / unavailable) and
  a quota bar (`quota_remaining` vs `quota_limit`);
- create/edit via modal forms; delete with a confirm (provider delete warns when routes
  reference it);
- token prompt persisted in `localStorage`.

Built with the frontend-design skill for a distinctive, non-generic look.

## Testing

- **Unit (TDD), `store.rs`**: insert→get roundtrip, update changes fields + preserves PK,
  delete removes row, provider delete cascades routes, duplicate-name insert errors,
  update of absent row reports 0 rows. Matches the existing `store::tests` style with a
  temp pool.
- **Manual / end-to-end verify**: run the server; curl create/list/update/delete for a
  provider and a route; confirm the swapped config takes effect (new route routes a
  request; deleted provider drops from `/v1/providers`); confirm the watcher reconciles
  (a newly-added probe-enabled provider starts getting probed); load the page and exercise
  each action in a browser.

## Files touched

- `migrations/` — none (schema already supports everything).
- `src/store.rs` — CRUD fns + tests.
- `src/config.rs` — `Provider` may need `Serialize` for admin GET (or build JSON by hand;
  decide at impl — leaning hand-built to control masking).
- `src/watcher.rs` — `spawn` returns handles; add `reconcile`.
- `src/server.rs` — `AppState` change, snapshot in handlers, `check_auth(&Config)`,
  `reload_from_db`, admin handlers, serve page, wire routes.
- `src/admin.rs` (new) — admin handlers + DTOs (keeps `server.rs` focused).
- `src/main.rs` — construct `RwLock<Arc<Config>>` + store watcher handles in `AppState`.
- `web/admin.html` (new) — the dashboard.
- `src/lib.rs` — `pub mod admin;`.
