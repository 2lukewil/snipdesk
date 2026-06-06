//! Background tombstone purge.
//!
//! Both personal_snippets and library_snippets carry an `is_deleted`
//! flag instead of dropping the row outright. Tombstones are how we
//! tell other clients that the row is gone - they pull "is_deleted =
//! true" on their next sync and apply the delete locally. The
//! tradeoff is that tombstones accumulate forever otherwise.
//!
//! This module spawns a long-lived tokio task that, once an hour,
//! deletes rows where `is_deleted = 1` and `updated_at` is older than
//! the configured retention window (default 90 days). The window has
//! to be longer than the longest plausible offline period for any
//! client; 90 days is the safe v1 default for an internal tool.
//!
//! Setting `tombstone_retention_days = 0` disables the purge entirely
//! (useful for development or environments that handle retention
//! externally).

use std::time::Duration;

use sqlx::SqlitePool;

/// How often the purge task wakes up to check. An hour is the right
/// granularity: precision is irrelevant at the 90-day scale, and a
/// shorter cadence would just add load.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Spawn the purge background task. Returns immediately; the actual
/// work runs in a tokio task that lives until process shutdown.
pub fn spawn(pool: SqlitePool, retention_days: u32) {
    if retention_days == 0 {
        tracing::info!("tombstone purge disabled (retention_days = 0)");
        return;
    }
    tracing::info!(
        retention_days,
        "tombstone purge task starting (will sweep hourly)"
    );
    tokio::spawn(async move {
        // Initial pause so startup isn't competing with the first
        // wave of client syncs. An hour is fine - if we just booted
        // and there are 90-day-old tombstones, an hour more doesn't
        // matter.
        tokio::time::sleep(SWEEP_INTERVAL).await;
        loop {
            if let Err(e) = sweep_once(&pool, retention_days).await {
                tracing::warn!(error = %e, "tombstone purge sweep failed; will retry next cycle");
            }
            tokio::time::sleep(SWEEP_INTERVAL).await;
        }
    });
}

/// Run one purge pass. Public so tests can exercise the same path
/// without the hourly loop.
pub async fn sweep_once(pool: &SqlitePool, retention_days: u32) -> anyhow::Result<()> {
    let cutoff = chrono::Utc::now().timestamp() - (retention_days as i64) * 86_400;

    // Personal snippets: tombstones older than cutoff disappear.
    // We DELETE rather than VACUUM so the row is gone from queries
    // immediately; SQLite reuses the freed pages on its own schedule.
    let personal = sqlx::query(
        "DELETE FROM personal_snippets \
         WHERE is_deleted = 1 AND updated_at < ?",
    )
    .bind(cutoff)
    .execute(pool)
    .await?;

    let library = sqlx::query(
        "DELETE FROM library_snippets \
         WHERE is_deleted = 1 AND updated_at < ?",
    )
    .bind(cutoff)
    .execute(pool)
    .await?;

    let total = personal.rows_affected() + library.rows_affected();
    if total > 0 {
        tracing::info!(
            personal = personal.rows_affected(),
            library = library.rows_affected(),
            cutoff,
            "tombstone purge swept"
        );
    } else {
        tracing::debug!(cutoff, "tombstone purge: nothing to drop");
    }
    Ok(())
}
