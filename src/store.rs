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
        .create_if_missing(true);
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
              probe_script, probe_enabled, probe_interval_secs, quota_min) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    let prows: Vec<ProviderRow> = sqlx::query_as(
        "SELECT name, wire, base_url, api_key, model_prefix, max_tokens_field, extra_headers, \
                probe_script, probe_enabled, probe_interval_secs, quota_min \
         FROM providers",
    )
    .fetch_all(pool)
    .await?;
    let mut providers = HashMap::with_capacity(prows.len());
    for r in prows {
        let extra_headers: HashMap<String, String> =
            serde_json::from_str(&r.extra_headers).unwrap_or_default();
        providers.insert(
            r.name,
            Provider {
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
            },
        );
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
}

#[derive(sqlx::FromRow)]
struct RouteRow {
    alias: String,
    provider: String,
    model: String,
    fallback: String,
}

fn wire_to_str(w: WireName) -> &'static str {
    match w {
        WireName::OpenaiChat => "openai-chat",
        WireName::OpenaiResponses => "openai-responses",
        WireName::AnthropicMessages => "anthropic-messages",
    }
}

fn str_to_wire(s: &str) -> anyhow::Result<WireName> {
    Ok(match s {
        "openai-chat" => WireName::OpenaiChat,
        "openai-responses" => WireName::OpenaiResponses,
        "anthropic-messages" => WireName::AnthropicMessages,
        other => anyhow::bail!("unknown wire format in DB: {other}"),
    })
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
