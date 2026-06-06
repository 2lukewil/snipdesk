//! snipdesk-server — self-hostable backend for SnipDesk Teams.
//!
//! Currently a phase-1 scaffold: config + master-key validation + SQLite
//! migrations + an Axum `/api/health` endpoint. Auth, snippet sync, and
//! the dashboard come in subsequent phases (see docs/server-design.md).

mod config;
mod db;
mod http;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "snipdesk-server", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Path to the TOML config file. Used when no subcommand is given
    /// (i.e. the default `run` action).
    #[arg(long, short = 'c', default_value = "snipdesk-server.toml")]
    config: PathBuf,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Boot the server. Default action if no subcommand is supplied.
    Run,
    /// Generate a fresh 256-bit master encryption key (base64). Pipe into
    /// your secret store; never commit the result.
    GenKey,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.cmd.unwrap_or(Cmd::Run) {
        Cmd::GenKey => {
            let key = config::MasterKey::generate();
            println!("{}", key.to_base64());
            Ok(())
        }
        Cmd::Run => run(cli.config).await,
    }
}

async fn run(config_path: PathBuf) -> Result<()> {
    let cfg = config::Config::load(&config_path)
        .with_context(|| format!("load config {}", config_path.display()))?;
    let master_key = config::load_master_key(&cfg.crypto)?;
    tracing::info!(
        bind_addr = %cfg.bind_addr,
        data_dir = %cfg.data_dir.display(),
        "master key loaded; preparing database"
    );

    let pool = db::open(&cfg.data_dir).await?;

    let state = http::AppState {
        pool,
        master_key: Arc::new(master_key),
    };
    let app = http::router(state);

    let listener = TcpListener::bind(&cfg.bind_addr)
        .await
        .with_context(|| format!("bind {}", cfg.bind_addr))?;
    tracing::info!("snipdesk-server listening on {}", cfg.bind_addr);
    axum::serve(listener, app).await.context("axum serve")?;
    Ok(())
}

/// JSON logs to stdout, level filterable via RUST_LOG. JSON output makes
/// downstream log shippers (Vector, Loki, Datadog) parse fields without
/// regex; humans tailing locally can use `| jq`.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,tower_http=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .json()
        .init();
}
