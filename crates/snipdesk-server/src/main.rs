//! snipdesk-server - self-hostable backend for SnipDesk Teams.
//!
//! Currently a phase-1 scaffold: config + master-key validation + SQLite
//! migrations + an Axum `/api/health` endpoint. Auth, snippet sync, and
//! the dashboard come in subsequent phases (see docs/server-design.md).

use std::path::PathBuf;
use std::sync::Arc;

use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use snipdesk_server::{auth, cli as cli_cmds, config, console, db, http, purge};
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
    Run {
        /// Force the interactive console on, regardless of TTY
        /// detection. Useful in terminals that report `is_terminal()
        /// == false` despite being interactive - most notably MSYS /
        /// Git Bash / mintty on Windows, where stdio runs through
        /// pipes rather than real Win32 console handles.
        #[arg(long, conflicts_with = "no_console")]
        console: bool,
        /// Force the interactive console off, even when stdin looks
        /// like a TTY. Useful when piping commands at startup is
        /// undesirable, e.g. running under a supervisor that
        /// momentarily attaches a TTY.
        #[arg(long, conflicts_with = "console")]
        no_console: bool,
    },
    /// Generate a fresh 256-bit master encryption key (base64). Pipe into
    /// your secret store; never commit the result.
    GenKey,
    /// Generate a fresh 256-bit HS256 JWT secret (base64). Pipe into your
    /// secret store; rotate to invalidate every active session at once.
    GenJwtSecret,
    /// User-management commands. Reads `data_dir` from the same config
    /// file as `run`, so the CLI hits the same SQLite database the
    /// server uses. Safe to run while the server is up - WAL mode
    /// handles concurrent reader + writer.
    Users {
        #[command(subcommand)]
        cmd: cli_cmds::UsersCmd,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    // Default action when no subcommand is supplied is `run` with no
    // console override; explicit `run --console` etc. takes the
    // matched arm below.
    match cli.cmd.unwrap_or(Cmd::Run {
        console: false,
        no_console: false,
    }) {
        Cmd::GenKey => {
            let key = config::MasterKey::generate();
            println!("{}", key.to_base64());
            Ok(())
        }
        Cmd::GenJwtSecret => {
            println!("{}", auth::generate_jwt_secret());
            Ok(())
        }
        Cmd::Run {
            console,
            no_console,
        } => {
            let force = if console {
                Some(true)
            } else if no_console {
                Some(false)
            } else {
                None
            };
            run(cli.config, force).await
        }
        Cmd::Users { cmd } => {
            // Reuse the same config so `data_dir` is one source of truth
            // - the CLI hits whichever DB the server is configured for.
            let cfg = config::Config::load(&cli.config).with_context(|| {
                format!("load config {} for users subcommand", cli.config.display())
            })?;
            cli_cmds::run(&cfg.data_dir, cmd).await
        }
    }
}

/// Boot the HTTP server and (optionally) the interactive console.
///
/// `force_console`:
///   - `Some(true)` - always start the console (caller swore stdin is
///     interactive; useful for Git Bash / mintty).
///   - `Some(false)` - never start the console, even on a TTY.
///   - `None` - auto-detect via `is_terminal()`.
async fn run(config_path: PathBuf, force_console: Option<bool>) -> Result<()> {
    let cfg = config::Config::load(&config_path)
        .with_context(|| format!("load config {}", config_path.display()))?;
    let master_key = config::load_master_key(&cfg.crypto)?;
    tracing::info!(
        bind_addr = %cfg.bind_addr,
        data_dir = %cfg.data_dir.display(),
        "master key loaded; preparing database"
    );

    let pool = db::open(&cfg.data_dir).await?;

    let fx_cache = Arc::new(snipdesk_server::fx::FxCache::default());
    // Live FX is opt-in via [fx] in the TOML. When unset, the
    // dashboard's currency conversion uses the static aud_rates
    // table; no outbound HTTP from the server.
    if let Some(fx_cfg) = cfg.fx.clone() {
        tracing::info!(
            provider = %fx_cfg.provider,
            ttl_hours = fx_cfg.cache_ttl_hours,
            "fx: live currency feed enabled"
        );
        snipdesk_server::fx::spawn_refresher(fx_cfg, fx_cache.clone());
    } else {
        tracing::info!("fx: live feed disabled (no [fx] in config); using static aud_rates");
    }
    let state = http::AppState {
        // Clone the pool so the interactive console can borrow it
        // alongside the HTTP handlers. sqlx pools are Arc-internal, so
        // clones share the same underlying connections.
        pool: pool.clone(),
        master_key: Arc::new(master_key),
        jwt_secret: cfg.jwt_secret.clone().unwrap_or_default(),
        oidc_google: cfg.oidc.google.clone(),
        secure_cookies: cfg.secure_cookies,
        stats: cfg.stats.clone(),
        fx_cache,
        cors_allowed_origins: cfg.cors_allowed_origins.clone(),
        brand_name: cfg.brand.name.clone(),
    };
    if state.jwt_secret.is_empty() {
        tracing::warn!(
            "jwt_secret not set in config - /api/auth/* and /api/me will 500 \
             until you set one. Generate with: snipdesk-server gen-jwt-secret"
        );
    }
    let app = http::router(state);

    let listener = TcpListener::bind(&cfg.bind_addr)
        .await
        .with_context(|| format!("bind {}", cfg.bind_addr))?;
    tracing::info!("snipdesk-server listening on {}", cfg.bind_addr);

    // Spawn the tombstone-purge sweep. Self-disabling when retention
    // is set to 0; otherwise hourly while the process lives.
    purge::spawn(pool.clone(), cfg.tombstone_retention_days);

    // Optional interactive console. Spawned when stdin is a real
    // terminal, OR when the operator explicitly forces it via
    // `--console` (useful for Git Bash / mintty / other pty wrappers
    // that confuse `is_terminal()`). Suppressed by `--no-console` or
    // when stdin is clearly non-interactive (systemd, docker without
    // `-it`, a CI runner, etc.).
    let want_console = match force_console {
        Some(v) => v,
        None => std::io::stdin().is_terminal(),
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    if want_console {
        let pool_for_console = pool.clone();
        tokio::spawn(async move {
            console::run(pool_for_console, shutdown_tx).await;
        });
    } else {
        // Sender forgotten (not dropped) so the receiver pends forever
        // - the server then runs until Ctrl+C / SIGTERM. Dropping it
        // would resolve `with_graceful_shutdown` immediately and the
        // server would exit before accepting a connection.
        std::mem::forget(shutdown_tx);
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await
        .context("axum serve")?;
    Ok(())
}

/// Logs to stdout, level filterable via RUST_LOG.
///
/// Two output formats:
///   - **TTY mode** (developer running the server in a terminal): a
///     compact human-readable format, so console interaction reads
///     like a normal interactive session. Log lines still interleave
///     with the user's typing - Minecraft-style - which is acceptable
///     for the low log volume this server produces.
///   - **Non-TTY mode** (systemd, docker, CI): JSON, one event per
///     line, so log shippers (Vector, Loki, Datadog) can parse fields
///     without regex.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,tower_http=info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    if std::io::stdout().is_terminal() {
        builder.compact().init();
    } else {
        builder.json().init();
    }
}
