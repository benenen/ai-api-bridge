use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

use std::sync::RwLock;

use ai_api_bridge::config::Config;
use ai_api_bridge::server::{AppState, build_app};
use ai_api_bridge::upstream::Upstream;
use ai_api_bridge::{store, watcher};

#[derive(Parser, Debug)]
#[command(name = "ai-api-bridge")]
struct Cli {
    /// Path to the bridge config file
    #[arg(long, default_value = "bridge.toml")]
    config: PathBuf,
    /// Override the listen address (host:port)
    #[arg(long)]
    listen: Option<String>,
    /// Override the SQLite database path (default: `database` in the config file)
    #[arg(long)]
    db: Option<String>,
    /// Wipe the providers/routes tables and re-import them from the config file
    #[arg(long)]
    reseed: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ai_api_bridge=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let mut config = Config::load(&cli.config)?;
    if let Some(listen) = cli.listen {
        config.listen = listen;
    }
    let addr = config.listen.clone();

    // Providers + routes live in SQLite. bridge.toml seeds an empty DB once; the DB
    // is authoritative thereafter. Env api-key overrides are applied last so secrets
    // supplied via env never get persisted to the DB.
    let db_path = cli.db.unwrap_or_else(|| config.database.clone());
    let pool = store::open(&db_path).await?;
    if cli.reseed {
        store::clear(&pool).await?;
        tracing::info!("--reseed: cleared providers/routes, re-importing from config");
    }
    if store::is_empty(&pool).await? {
        store::seed_from_config(&pool, &config).await?;
        tracing::info!(
            providers = config.providers.len(),
            routes = config.routes.len(),
            "seeded providers/routes from {}",
            cli.config.display()
        );
    }
    store::load_into_config(&pool, &mut config).await?;
    config.apply_env_overrides();
    tracing::info!(
        db = %db_path,
        providers = config.providers.len(),
        routes = config.routes.len(),
        "loaded providers/routes from DB"
    );

    // Provider watcher: seed the in-memory status map from the DB, then spawn a
    // background probe per enabled provider (keeps the pool alive for writes).
    let status: watcher::StatusMap = Arc::new(RwLock::new(
        store::load_statuses(&pool).await.unwrap_or_default(),
    ));
    watcher::spawn(pool.clone(), &config.providers, status.clone());

    let state = Arc::new(AppState {
        config,
        upstream: Upstream::new(),
        status,
        pool: Some(pool),
    });
    let app = build_app(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("ai-api-bridge listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
