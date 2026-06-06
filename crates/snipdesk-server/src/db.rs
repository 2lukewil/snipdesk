//! SQLite connection setup and migration runner.
//!
//! sqlx is async; the rest of the server is async; the database lives on
//! local disk so latency is bounded. The connection pool is small by
//! default (single SQLite file, no real concurrency benefit past a few
//! readers) and is cheap to clone across handlers via the Axum state.

use std::path::Path;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use sqlx::ConnectOptions;
use std::str::FromStr;

/// Open (or create) the SQLite DB at `<data_dir>/snipdesk.db` and run
/// pending migrations. WAL keeps reads non-blocking during writes — a
/// substantial win even on this small workload.
pub async fn open(data_dir: &Path) -> Result<SqlitePool> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("create data_dir {}", data_dir.display()))?;
    let db_path = data_dir.join("snipdesk.db");
    let url = format!("sqlite://{}", db_path.display());

    let opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        // sqlx is chatty at INFO; drop to warn for noisy SQL statements.
        .log_statements(tracing::log::LevelFilter::Debug);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .with_context(|| format!("connect to {url}"))?;

    run_migrations(&pool).await?;
    Ok(pool)
}

/// Apply the embedded migrations to a pool. Split out so tests can call
/// it against an in-memory pool without the data_dir / WAL setup above.
pub async fn run_migrations(pool: &SqlitePool) -> Result<()> {
    // Embed migrations at compile time so the binary is fully
    // self-contained — operators don't need to ship a migrations folder
    // alongside the executable.
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .context("run migrations")?;
    Ok(())
}
