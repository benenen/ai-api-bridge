mod canonical;
mod config;
mod error;
mod router;
mod server;
mod sse;
mod upstream;
mod wire;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

use crate::config::Config;
use crate::server::{build_app, AppState};

#[derive(Parser, Debug)]
#[command(name = "ai-api-bridge")]
struct Cli {
    /// Path to the bridge config file
    #[arg(long, default_value = "bridge.toml")]
    config: PathBuf,
    /// Override the listen address (host:port)
    #[arg(long)]
    listen: Option<String>,
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

    let state = Arc::new(AppState { config });
    let app = build_app(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("ai-api-bridge listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
