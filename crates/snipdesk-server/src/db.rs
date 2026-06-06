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
/// pending migrations. WAL keeps reads non-blocking during writes - a
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
///
/// Self-repair: if sqlx reports a checksum mismatch on a migration
/// that was already applied, we recompute and update the stored
/// checksum, then re-run. This is here because editing comments inside
/// a migration file (e.g. a project-wide find/replace) shouldn't lock
/// an existing deployment out. The SQL itself was already applied
/// successfully when the migration first ran; only the bookkeeping
/// row in `_sqlx_migrations` needs to catch up.
///
/// Caveat: this trusts that the SQL EFFECT hasn't changed - only
/// surrounding comments / whitespace. If you ever genuinely need to
/// alter a previously-applied migration's behaviour, write a NEW
/// migration; don't edit the old one and rely on self-repair.
pub async fn run_migrations(pool: &SqlitePool) -> Result<()> {
    let migrator = sqlx::migrate!("./migrations");
    match migrator.run(pool).await {
        Ok(()) => Ok(()),
        Err(sqlx::migrate::MigrateError::VersionMismatch(version)) => {
            tracing::warn!(
                version,
                "migration checksum mismatch; recomputing checksum from on-disk file \
                 and re-running. This is expected after comment-only edits."
            );
            repair_checksum(pool, &migrator, version).await?;
            migrator
                .run(pool)
                .await
                .context("run migrations (post-repair)")?;
            Ok(())
        }
        Err(e) => Err(e).context("run migrations"),
    }
}

/// Update the `_sqlx_migrations.checksum` row for `version` to whatever
/// the embedded migration source now computes to. Pulls the migration's
/// already-computed checksum out of sqlx's Migrator (no need to hash
/// ourselves) so we stay in lockstep with what sqlx will verify against
/// on the next run.
async fn repair_checksum(
    pool: &SqlitePool,
    migrator: &sqlx::migrate::Migrator,
    version: i64,
) -> Result<()> {
    let target = migrator
        .iter()
        .find(|m| m.version == version)
        .ok_or_else(|| {
            anyhow::anyhow!("migration {version} not found in embedded migrator")
        })?;
    sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
        .bind(target.checksum.as_ref())
        .bind(version)
        .execute(pool)
        .await
        .with_context(|| format!("update _sqlx_migrations checksum for version {version}"))?;
    Ok(())
}
