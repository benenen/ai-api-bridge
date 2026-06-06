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
             (name, wire, base_url, api_key, model_prefix, max_tokens_field, extra_headers) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(name.as_str())
        .bind(wire_to_str(p.wire))
        .bind(p.base_url.as_str())
        .bind(p.api_key.as_deref())
        .bind(p.model_prefix.as_deref())
        .bind(p.max_tokens_field.as_str())
        .bind(serde_json::to_string(&p.extra_headers)?)
        .execute(&mut *tx)
        .await?;
    }
    for r in &cfg.routes {
        sqlx::query("INSERT INTO routes (alias, provider, model) VALUES (?, ?, ?)")
            .bind(r.alias.as_str())
            .bind(r.provider.as_str())
            .bind(r.model.as_str())
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
        "SELECT name, wire, base_url, api_key, model_prefix, max_tokens_field, extra_headers \
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
            },
        );
    }

    let rrows: Vec<RouteRow> = sqlx::query_as("SELECT alias, provider, model FROM routes")
        .fetch_all(pool)
        .await?;
    let routes = rrows
        .into_iter()
        .map(|r| Route {
            alias: r.alias,
            provider: r.provider,
            model: r.model,
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
}

#[derive(sqlx::FromRow)]
struct RouteRow {
    alias: String,
    provider: String,
    model: String,
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
}
