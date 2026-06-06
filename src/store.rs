//! SQLite-backed store for providers + routes (sqlx, runtime queries).
//!
//! `bridge.toml` seeds an empty DB on first run; thereafter the DB is the source
//! of truth — loaded into the in-memory `Config` at startup (no per-request DB hit).

use std::collections::HashMap;

use anyhow::Context;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

use crate::config::{Config, Provider, Route, WireName};

/// Open (creating if missing) the SQLite database and run migrations.
pub async fn open(path: &str) -> anyhow::Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        // Enforce `routes.provider`/`provider_status.provider` FKs so deleting a
        // provider cascades to its routes + status row (admin delete relies on this).
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
        .with_context(|| format!("opening sqlite db {path}"))?;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("running migrations")?;
    Ok(pool)
}

/// True when no providers are stored yet (i.e. the DB needs seeding).
pub async fn is_empty(pool: &SqlitePool) -> anyhow::Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM providers")
        .fetch_one(pool)
        .await?;
    Ok(n == 0)
}

/// Insert the config's providers + routes into the DB (first-run seed / `--reseed`).
pub async fn seed_from_config(pool: &SqlitePool, cfg: &Config) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    for (name, p) in &cfg.providers {
        sqlx::query(
            "INSERT INTO providers \
             (name, wire, base_url, api_key, model_prefix, max_tokens_field, extra_headers, \
              probe_script, probe_enabled, probe_interval_secs, quota_min, cost_windows, \
              model_prices, usage) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(name.as_str())
        .bind(wire_to_str(p.wire))
        .bind(p.base_url.as_str())
        .bind(p.api_key.as_deref())
        .bind(p.model_prefix.as_deref())
        .bind(p.max_tokens_field.as_str())
        .bind(serde_json::to_string(&p.extra_headers)?)
        .bind(p.probe_script.as_deref())
        .bind(p.probe_enabled.map(|b| b as i64))
        .bind(p.probe_interval_secs.map(|s| s as i64))
        .bind(p.quota_min)
        .bind(serde_json::to_string(&p.cost_windows)?)
        .bind(serde_json::to_string(&p.model_prices)?)
        .bind(serde_json::to_string(&p.usage)?)
        .execute(&mut *tx)
        .await?;
    }
    for r in &cfg.routes {
        sqlx::query("INSERT INTO routes (alias, provider, model, fallback) VALUES (?, ?, ?, ?)")
            .bind(r.alias.as_str())
            .bind(r.provider.as_str())
            .bind(r.model.as_str())
            .bind(serde_json::to_string(&r.fallback)?)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Delete all providers + routes (used by `--reseed` before re-importing).
pub async fn clear(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM routes").execute(pool).await?;
    sqlx::query("DELETE FROM providers").execute(pool).await?;
    Ok(())
}

/// Replace `cfg.providers` + `cfg.routes` with the DB contents.
pub async fn load_into_config(pool: &SqlitePool, cfg: &mut Config) -> anyhow::Result<()> {
    let prows: Vec<ProviderRow> = sqlx::query_as(&format!("SELECT {PROVIDER_COLS} FROM providers"))
        .fetch_all(pool)
        .await?;
    let mut providers = HashMap::with_capacity(prows.len());
    for r in prows {
        providers.insert(r.name.clone(), row_to_provider(r)?);
    }

    let rrows: Vec<RouteRow> =
        sqlx::query_as("SELECT alias, provider, model, fallback FROM routes")
            .fetch_all(pool)
            .await?;
    let routes = rrows
        .into_iter()
        .map(|r| Route {
            alias: r.alias,
            provider: r.provider,
            model: r.model,
            fallback: serde_json::from_str(&r.fallback).unwrap_or_default(),
        })
        .collect();

    cfg.providers = providers;
    cfg.routes = routes;
    Ok(())
}

fn row_to_provider(r: ProviderRow) -> anyhow::Result<Provider> {
    let extra_headers: HashMap<String, String> =
        serde_json::from_str(&r.extra_headers).unwrap_or_default();
    let mut p = Provider {
        wire: str_to_wire(&r.wire)?,
        base_url: r.base_url,
        api_key: r.api_key,
        model_prefix: r.model_prefix,
        max_tokens_field: r.max_tokens_field,
        extra_headers,
        probe_script: r.probe_script,
        probe_enabled: r.probe_enabled.map(|b| b != 0),
        probe_interval_secs: r.probe_interval_secs.map(|s| s as u64),
        quota_min: r.quota_min,
        cost_windows: serde_json::from_str(&r.cost_windows).unwrap_or_default(),
        model_prices: serde_json::from_str(&r.model_prices).unwrap_or_default(),
        usage: serde_json::from_str(&r.usage).unwrap_or_default(),
    };
    p.normalize_usage(); // fold legacy cost_windows/model_prices when `usage` is empty
    Ok(p)
}

const PROVIDER_COLS: &str = "name, wire, base_url, api_key, model_prefix, max_tokens_field, \
     extra_headers, probe_script, probe_enabled, probe_interval_secs, quota_min, cost_windows, \
     model_prices, usage";

/// Fetch one provider by name (used by the admin update path to read the stored
/// `api_key` when the form leaves it blank).
pub async fn get_provider(pool: &SqlitePool, name: &str) -> anyhow::Result<Option<Provider>> {
    let row: Option<ProviderRow> = sqlx::query_as(&format!(
        "SELECT {PROVIDER_COLS} FROM providers WHERE name = ?"
    ))
    .bind(name)
    .fetch_optional(pool)
    .await?;
    row.map(row_to_provider).transpose()
}

/// Insert a new provider. Errors (UNIQUE violation) if `name` already exists —
/// callers check `get_provider` first to return a clean 409.
pub async fn insert_provider(pool: &SqlitePool, name: &str, p: &Provider) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO providers \
         (name, wire, base_url, api_key, model_prefix, max_tokens_field, extra_headers, \
          probe_script, probe_enabled, probe_interval_secs, quota_min, cost_windows, \
          model_prices, usage) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(name)
    .bind(wire_to_str(p.wire))
    .bind(p.base_url.as_str())
    .bind(p.api_key.as_deref())
    .bind(p.model_prefix.as_deref())
    .bind(p.max_tokens_field.as_str())
    .bind(serde_json::to_string(&p.extra_headers)?)
    .bind(p.probe_script.as_deref())
    .bind(p.probe_enabled.map(|b| b as i64))
    .bind(p.probe_interval_secs.map(|s| s as i64))
    .bind(p.quota_min)
    .bind(serde_json::to_string(&p.cost_windows)?)
    .bind(serde_json::to_string(&p.model_prices)?)
    .bind(serde_json::to_string(&p.usage)?)
    .execute(pool)
    .await?;
    Ok(())
}

/// Update an existing provider in place (`name` is the immutable PK). Returns the
/// number of rows affected (0 = no such provider).
pub async fn update_provider(pool: &SqlitePool, name: &str, p: &Provider) -> anyhow::Result<u64> {
    let res = sqlx::query(
        "UPDATE providers SET \
           wire = ?, base_url = ?, api_key = ?, model_prefix = ?, max_tokens_field = ?, \
           extra_headers = ?, probe_script = ?, probe_enabled = ?, probe_interval_secs = ?, \
           quota_min = ?, cost_windows = ?, model_prices = ?, usage = ? \
         WHERE name = ?",
    )
    .bind(wire_to_str(p.wire))
    .bind(p.base_url.as_str())
    .bind(p.api_key.as_deref())
    .bind(p.model_prefix.as_deref())
    .bind(p.max_tokens_field.as_str())
    .bind(serde_json::to_string(&p.extra_headers)?)
    .bind(p.probe_script.as_deref())
    .bind(p.probe_enabled.map(|b| b as i64))
    .bind(p.probe_interval_secs.map(|s| s as i64))
    .bind(p.quota_min)
    .bind(serde_json::to_string(&p.cost_windows)?)
    .bind(serde_json::to_string(&p.model_prices)?)
    .bind(serde_json::to_string(&p.usage)?)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Delete a provider. `ON DELETE CASCADE` removes its routes + status row. Returns
/// rows affected (0 = no such provider).
pub async fn delete_provider(pool: &SqlitePool, name: &str) -> anyhow::Result<u64> {
    let res = sqlx::query("DELETE FROM providers WHERE name = ?")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Insert a new route. Errors (UNIQUE violation) if `alias` already exists.
pub async fn insert_route(pool: &SqlitePool, r: &Route) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO routes (alias, provider, model, fallback) VALUES (?, ?, ?, ?)")
        .bind(r.alias.as_str())
        .bind(r.provider.as_str())
        .bind(r.model.as_str())
        .bind(serde_json::to_string(&r.fallback)?)
        .execute(pool)
        .await?;
    Ok(())
}

/// Update an existing route (`alias` is the immutable PK). Returns rows affected.
pub async fn update_route(pool: &SqlitePool, alias: &str, r: &Route) -> anyhow::Result<u64> {
    let res =
        sqlx::query("UPDATE routes SET provider = ?, model = ?, fallback = ? WHERE alias = ?")
            .bind(r.provider.as_str())
            .bind(r.model.as_str())
            .bind(serde_json::to_string(&r.fallback)?)
            .bind(alias)
            .execute(pool)
            .await?;
    Ok(res.rows_affected())
}

/// Delete a route by alias. Returns rows affected (0 = no such route).
pub async fn delete_route(pool: &SqlitePool, alias: &str) -> anyhow::Result<u64> {
    let res = sqlx::query("DELETE FROM routes WHERE alias = ?")
        .bind(alias)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Append one typed usage event (per-request amount in the kind's unit).
pub async fn insert_usage_event(
    pool: &SqlitePool,
    provider: &str,
    ts: i64,
    usage_type: &str,
    amount: f64,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO usage_events (provider, ts, usage_type, amount) VALUES (?, ?, ?, ?)")
        .bind(provider)
        .bind(ts)
        .bind(usage_type)
        .bind(amount)
        .execute(pool)
        .await?;
    Ok(())
}

/// Load usage events at or after `since` — `(provider, ts, usage_type, amount)`.
/// Seeds the in-memory meter at startup.
pub async fn load_usage_events(
    pool: &SqlitePool,
    since: i64,
) -> anyhow::Result<Vec<(String, i64, String, f64)>> {
    let rows: Vec<UsageRow> = sqlx::query_as(
        "SELECT provider, ts, usage_type, amount FROM usage_events WHERE ts >= ? ORDER BY ts",
    )
    .bind(since)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.provider, r.ts, r.usage_type, r.amount))
        .collect())
}

/// Delete usage events older than `before` (retention pruning). Returns rows removed.
pub async fn prune_usage_events(pool: &SqlitePool, before: i64) -> anyhow::Result<u64> {
    let res = sqlx::query("DELETE FROM usage_events WHERE ts < ?")
        .bind(before)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

#[derive(sqlx::FromRow)]
struct UsageRow {
    provider: String,
    ts: i64,
    usage_type: String,
    amount: f64,
}

#[derive(sqlx::FromRow)]
struct ProviderRow {
    name: String,
    wire: String,
    base_url: String,
    api_key: Option<String>,
    model_prefix: Option<String>,
    max_tokens_field: String,
    extra_headers: String,
    probe_script: Option<String>,
    probe_enabled: Option<i64>,
    probe_interval_secs: Option<i64>,
    quota_min: Option<f64>,
    cost_windows: String,
    model_prices: String,
    usage: String,
}

#[derive(sqlx::FromRow)]
struct RouteRow {
    alias: String,
    provider: String,
    model: String,
    fallback: String,
}

fn wire_to_str(w: WireName) -> &'static str {
    w.as_str()
}

fn str_to_wire(s: &str) -> anyhow::Result<WireName> {
    WireName::parse(s).ok_or_else(|| anyhow::anyhow!("unknown wire format in DB: {s}"))
}

/// Runtime watcher state for one provider (written by the watcher; read by the
/// status endpoint and the failover router).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ProviderStatus {
    pub available: bool,
    pub quota_remaining: Option<f64>,
    pub quota_used: Option<f64>,
    pub quota_limit: Option<f64>,
    pub last_checked: Option<i64>,
    pub last_ok: bool,
    pub error: Option<String>,
    pub note: Option<String>,
}

#[derive(sqlx::FromRow)]
struct StatusRow {
    provider: String,
    available: i64,
    quota_remaining: Option<f64>,
    quota_used: Option<f64>,
    quota_limit: Option<f64>,
    last_checked: Option<i64>,
    last_ok: i64,
    error: Option<String>,
    note: Option<String>,
}

/// Load all persisted provider statuses (seeds the in-memory map at startup).
pub async fn load_statuses(pool: &SqlitePool) -> anyhow::Result<HashMap<String, ProviderStatus>> {
    let rows: Vec<StatusRow> = sqlx::query_as(
        "SELECT provider, available, quota_remaining, quota_used, quota_limit, \
                last_checked, last_ok, error, note \
         FROM provider_status",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.provider,
                ProviderStatus {
                    available: r.available != 0,
                    quota_remaining: r.quota_remaining,
                    quota_used: r.quota_used,
                    quota_limit: r.quota_limit,
                    last_checked: r.last_checked,
                    last_ok: r.last_ok != 0,
                    error: r.error,
                    note: r.note,
                },
            )
        })
        .collect())
}

/// Upsert one provider's status.
pub async fn write_status(
    pool: &SqlitePool,
    provider: &str,
    s: &ProviderStatus,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO provider_status \
         (provider, available, quota_remaining, quota_used, quota_limit, last_checked, last_ok, error, note) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(provider) DO UPDATE SET \
           available=excluded.available, quota_remaining=excluded.quota_remaining, \
           quota_used=excluded.quota_used, quota_limit=excluded.quota_limit, \
           last_checked=excluded.last_checked, last_ok=excluded.last_ok, \
           error=excluded.error, note=excluded.note",
    )
    .bind(provider)
    .bind(s.available as i64)
    .bind(s.quota_remaining)
    .bind(s.quota_used)
    .bind(s.quota_limit)
    .bind(s.last_checked)
    .bind(s.last_ok as i64)
    .bind(s.error.as_deref())
    .bind(s.note.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    async fn temp_pool() -> SqlitePool {
        let path = std::env::temp_dir().join(format!("bridge-test-{}.db", uuid::Uuid::new_v4()));
        open(path.to_str().unwrap()).await.unwrap()
    }

    #[tokio::test]
    async fn seed_then_load_roundtrips() {
        let cfg = Config::from_toml(
            r#"
default_provider = "zen"
[providers.zen]
wire = "openai-chat"
base_url = "https://opencode.ai/zen/v1"
model_prefix = "opencode/"
[providers.go]
wire = "anthropic-messages"
base_url = "https://opencode.ai/zen/go/v1"
api_key = "sk-go"
extra_headers = { "x-foo" = "bar" }
[[routes]]
alias = "gpt-5.5"
provider = "go"
model = "deepseek-v4-pro"
"#,
        )
        .unwrap();

        let pool = temp_pool().await;
        assert!(is_empty(&pool).await.unwrap());
        seed_from_config(&pool, &cfg).await.unwrap();
        assert!(!is_empty(&pool).await.unwrap());

        let mut loaded = Config::from_toml("").unwrap();
        load_into_config(&pool, &mut loaded).await.unwrap();

        assert_eq!(loaded.providers.len(), 2);
        let go = loaded.providers.get("go").unwrap();
        assert_eq!(go.wire, WireName::AnthropicMessages);
        assert_eq!(go.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(go.api_key.as_deref(), Some("sk-go"));
        assert_eq!(
            go.extra_headers.get("x-foo").map(String::as_str),
            Some("bar")
        );
        let zen = loaded.providers.get("zen").unwrap();
        assert_eq!(zen.model_prefix.as_deref(), Some("opencode/"));
        assert_eq!(zen.max_tokens_field, "max_tokens");
        assert_eq!(loaded.routes.len(), 1);
        assert_eq!(loaded.routes[0].alias, "gpt-5.5");
        assert_eq!(loaded.routes[0].provider, "go");
        assert_eq!(loaded.routes[0].model, "deepseek-v4-pro");
    }

    #[tokio::test]
    async fn clear_empties_tables() {
        let cfg =
            Config::from_toml("[providers.zen]\nwire=\"openai-chat\"\nbase_url=\"u\"").unwrap();
        let pool = temp_pool().await;
        seed_from_config(&pool, &cfg).await.unwrap();
        assert!(!is_empty(&pool).await.unwrap());
        clear(&pool).await.unwrap();
        assert!(is_empty(&pool).await.unwrap());
    }

    #[tokio::test]
    async fn probe_config_and_fallback_roundtrip() {
        let cfg = Config::from_toml(
            r#"
default_provider = "go"
[providers.go]
wire = "openai-chat"
base_url = "https://opencode.ai/zen/go/v1"
probe_script = "probes/zen.lua"
probe_interval_secs = 60
quota_min = 1.5
[providers.zen]
wire = "openai-chat"
base_url = "https://opencode.ai/zen/v1"
[[routes]]
alias = "gpt-5.5"
provider = "go"
model = "deepseek-v4-pro"
fallback = [{ provider = "zen", model = "gpt-5.5" }]
"#,
        )
        .unwrap();
        let pool = temp_pool().await;
        seed_from_config(&pool, &cfg).await.unwrap();
        let mut loaded = Config::from_toml("").unwrap();
        load_into_config(&pool, &mut loaded).await.unwrap();

        let go = loaded.providers.get("go").unwrap();
        assert_eq!(go.probe_script.as_deref(), Some("probes/zen.lua"));
        assert_eq!(go.probe_interval_secs, Some(60));
        assert_eq!(go.quota_min, Some(1.5));
        assert!(go.probe_enabled()); // script set -> enabled
        assert!(!loaded.providers.get("zen").unwrap().probe_enabled()); // no script -> off

        let route = &loaded.routes[0];
        assert_eq!(route.fallback.len(), 1);
        assert_eq!(route.fallback[0].provider, "zen");
        assert_eq!(route.fallback[0].model, "gpt-5.5");
    }

    fn sample_provider(base_url: &str) -> Provider {
        Provider {
            wire: WireName::OpenaiChat,
            base_url: base_url.to_string(),
            api_key: Some("sk-orig".into()),
            model_prefix: Some("opencode/".into()),
            max_tokens_field: "max_tokens".into(),
            extra_headers: HashMap::from([("x-foo".to_string(), "bar".to_string())]),
            probe_script: Some("probes/zen.lua".into()),
            probe_enabled: Some(true),
            probe_interval_secs: Some(60),
            quota_min: Some(1.5),
            cost_windows: vec![crate::config::CostWindow {
                label: "5h".into(),
                window_secs: 18000,
                limit: 12.0,
            }],
            model_prices: HashMap::from([(
                "deepseek-v4-pro".to_string(),
                crate::config::ModelPrice {
                    input: 0.4,
                    output: 1.6,
                },
            )]),
            usage: Vec::new(),
        }
    }

    #[tokio::test]
    async fn provider_crud_roundtrip() {
        let pool = temp_pool().await;

        // insert + get
        insert_provider(&pool, "go", &sample_provider("https://go"))
            .await
            .unwrap();
        let got = get_provider(&pool, "go").await.unwrap().unwrap();
        assert_eq!(got.base_url, "https://go");
        assert_eq!(got.api_key.as_deref(), Some("sk-orig"));
        assert_eq!(
            got.extra_headers.get("x-foo").map(String::as_str),
            Some("bar")
        );
        assert_eq!(got.quota_min, Some(1.5));
        assert!(got.probe_enabled());
        assert_eq!(got.cost_windows.len(), 1);
        assert_eq!(got.cost_windows[0].label, "5h");
        assert_eq!(got.cost_windows[0].limit, 12.0);
        assert_eq!(got.model_prices.get("deepseek-v4-pro").unwrap().output, 1.6);

        // update changes fields, keeps the PK
        let mut updated = sample_provider("https://go-2");
        updated.api_key = Some("sk-new".into());
        updated.quota_min = Some(9.0);
        assert_eq!(update_provider(&pool, "go", &updated).await.unwrap(), 1);
        let got = get_provider(&pool, "go").await.unwrap().unwrap();
        assert_eq!(got.base_url, "https://go-2");
        assert_eq!(got.api_key.as_deref(), Some("sk-new"));
        assert_eq!(got.quota_min, Some(9.0));

        // update of an absent provider affects 0 rows
        assert_eq!(update_provider(&pool, "nope", &updated).await.unwrap(), 0);

        // delete removes the row
        assert_eq!(delete_provider(&pool, "go").await.unwrap(), 1);
        assert!(get_provider(&pool, "go").await.unwrap().is_none());
        assert_eq!(delete_provider(&pool, "go").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn insert_duplicate_provider_errors() {
        let pool = temp_pool().await;
        insert_provider(&pool, "go", &sample_provider("https://go"))
            .await
            .unwrap();
        assert!(
            insert_provider(&pool, "go", &sample_provider("https://go"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn delete_provider_cascades_routes_and_status() {
        let pool = temp_pool().await;
        insert_provider(&pool, "go", &sample_provider("https://go"))
            .await
            .unwrap();
        insert_route(
            &pool,
            &Route {
                alias: "gpt-5.5".into(),
                provider: "go".into(),
                model: "deepseek-v4-pro".into(),
                fallback: vec![],
            },
        )
        .await
        .unwrap();
        write_status(
            &pool,
            "go",
            &ProviderStatus {
                available: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        delete_provider(&pool, "go").await.unwrap();

        let mut loaded = Config::from_toml("").unwrap();
        load_into_config(&pool, &mut loaded).await.unwrap();
        assert!(loaded.providers.is_empty());
        assert!(loaded.routes.is_empty(), "route should cascade-delete");
        assert!(
            load_statuses(&pool).await.unwrap().is_empty(),
            "status should cascade-delete"
        );
    }

    #[tokio::test]
    async fn route_crud_roundtrip() {
        let pool = temp_pool().await;
        // routes FK-reference providers; create the providers first.
        insert_provider(&pool, "go", &sample_provider("https://go"))
            .await
            .unwrap();
        insert_provider(&pool, "zen", &sample_provider("https://zen"))
            .await
            .unwrap();

        insert_route(
            &pool,
            &Route {
                alias: "gpt-5.5".into(),
                provider: "go".into(),
                model: "deepseek-v4-pro".into(),
                fallback: vec![crate::config::RouteTarget {
                    provider: "zen".into(),
                    model: "gpt-5.5".into(),
                }],
            },
        )
        .await
        .unwrap();

        let load = |pool: SqlitePool| async move {
            let mut c = Config::from_toml("").unwrap();
            load_into_config(&pool, &mut c).await.unwrap();
            c.routes
        };

        let routes = load(pool.clone()).await;
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].provider, "go");
        assert_eq!(routes[0].fallback.len(), 1);

        // update model + fallback
        assert_eq!(
            update_route(
                &pool,
                "gpt-5.5",
                &Route {
                    alias: "gpt-5.5".into(),
                    provider: "zen".into(),
                    model: "gpt-5.5-mini".into(),
                    fallback: vec![],
                }
            )
            .await
            .unwrap(),
            1
        );
        let routes = load(pool.clone()).await;
        assert_eq!(routes[0].provider, "zen");
        assert_eq!(routes[0].model, "gpt-5.5-mini");
        assert!(routes[0].fallback.is_empty());

        // update absent -> 0; delete -> 1 then 0
        assert_eq!(
            update_route(
                &pool,
                "nope",
                &Route {
                    alias: "nope".into(),
                    provider: "zen".into(),
                    model: "x".into(),
                    fallback: vec![],
                }
            )
            .await
            .unwrap(),
            0
        );
        assert_eq!(delete_route(&pool, "gpt-5.5").await.unwrap(), 1);
        assert_eq!(delete_route(&pool, "gpt-5.5").await.unwrap(), 0);
        assert!(load(pool).await.is_empty());
    }

    #[tokio::test]
    async fn usage_events_insert_load_prune() {
        let pool = temp_pool().await;
        insert_usage_event(&pool, "go", 100, "billing", 1.5)
            .await
            .unwrap();
        insert_usage_event(&pool, "go", 200, "count", 1.0)
            .await
            .unwrap();
        insert_usage_event(&pool, "zen", 150, "billing", 0.5)
            .await
            .unwrap();

        let all = load_usage_events(&pool, 0).await.unwrap();
        assert_eq!(all.len(), 3);
        // tuple is (provider, ts, usage_type, amount)
        assert!(
            all.iter()
                .any(|(p, _, k, a)| p == "go" && k == "count" && *a == 1.0)
        );
        // since-filter: ts >= 150 keeps the 200 and 150 events
        assert_eq!(load_usage_events(&pool, 150).await.unwrap().len(), 2);
        // prune ts < 150 removes only the 100 event
        assert_eq!(prune_usage_events(&pool, 150).await.unwrap(), 1);
        assert_eq!(load_usage_events(&pool, 0).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn legacy_cost_windows_fold_to_billing_spec() {
        let pool = temp_pool().await;
        // sample_provider sets cost_windows + model_prices, usage empty -> folds on load.
        insert_provider(&pool, "go", &sample_provider("https://go"))
            .await
            .unwrap();
        let got = get_provider(&pool, "go").await.unwrap().unwrap();
        assert_eq!(got.usage.len(), 1);
        match &got.usage[0] {
            crate::config::UsageSpec::Billing {
                windows,
                model_prices,
            } => {
                assert_eq!(windows[0].label, "5h");
                assert!(model_prices.contains_key("deepseek-v4-pro"));
            }
            other => panic!("expected Billing spec, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_write_load_upsert() {
        let cfg =
            Config::from_toml("[providers.go]\nwire=\"openai-chat\"\nbase_url=\"u\"").unwrap();
        let pool = temp_pool().await;
        seed_from_config(&pool, &cfg).await.unwrap();
        write_status(
            &pool,
            "go",
            &ProviderStatus {
                available: true,
                quota_remaining: Some(12.5),
                last_checked: Some(100),
                last_ok: true,
                note: Some("ok".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // upsert overwrites the prior row
        write_status(
            &pool,
            "go",
            &ProviderStatus {
                available: false,
                quota_remaining: Some(0.0),
                last_ok: false,
                error: Some("boom".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let map = load_statuses(&pool).await.unwrap();
        let got = map.get("go").unwrap();
        assert!(!got.available);
        assert_eq!(got.quota_remaining, Some(0.0));
        assert_eq!(got.error.as_deref(), Some("boom"));
        assert!(got.note.is_none());
    }
}
