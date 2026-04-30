//! Daily SQLite snapshot + retention pruning.
//!
//! Plain file copy, not VACUUM INTO — DBs are tiny and we have no long
//! transactions. Snapshots are keyed by date (`snippets-YYYYMMDD.db`) so
//! repeated runs on the same day are idempotent. 6-hour cadence so always-on
//! installs cross day boundaries.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::logging;

/// Spawn the background snapshot loop. Call once from setup().
pub fn init_schedule(data_dir: &Path, db_path: &Path, retention_days: u32) {
    let data_dir = data_dir.to_path_buf();
    let db_path = db_path.to_path_buf();

    if let Err(err) = maybe_snapshot(&data_dir, &db_path, retention_days) {
        logging::log_error(&format!("backup: initial snapshot failed: {err}"));
    }

    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(6 * 60 * 60));
        if let Err(err) = maybe_snapshot(&data_dir, &db_path, retention_days) {
            logging::log_error(&format!("backup: scheduled snapshot failed: {err}"));
        }
    });
}

/// Snapshot if today's file doesn't exist. Always prunes, regardless.
fn maybe_snapshot(data_dir: &Path, db_path: &Path, retention_days: u32) -> std::io::Result<()> {
    let backups_dir = data_dir.join("backups");
    std::fs::create_dir_all(&backups_dir)?;

    let today_stem = today_stamp();
    let snapshot_path = backups_dir.join(format!("snippets-{today_stem}.db"));

    if !snapshot_path.exists() {
        match std::fs::copy(db_path, &snapshot_path) {
            Ok(bytes) => logging::log_info(&format!(
                "backup: snapshot {} ({} bytes)",
                snapshot_path.display(),
                bytes
            )),
            Err(err) => {
                logging::log_error(&format!(
                    "backup: failed to copy {} → {}: {err}",
                    db_path.display(),
                    snapshot_path.display()
                ));
                return Err(err);
            }
        }
    }

    prune_old(&backups_dir, retention_days);
    Ok(())
}

/// Drop `snippets-*.db` older than retention_days. Leaves unrelated files alone.
fn prune_old(backups_dir: &Path, retention_days: u32) {
    let Ok(entries) = std::fs::read_dir(backups_dir) else {
        return;
    };
    let cutoff_secs = retention_days as u64 * 24 * 60 * 60;
    let cutoff = SystemTime::now() - Duration::from_secs(cutoff_secs);

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with("snippets-") || !name.ends_with(".db") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified < cutoff {
            if let Err(err) = std::fs::remove_file(&path) {
                logging::log_error(&format!(
                    "backup: prune failed for {}: {err}",
                    path.display()
                ));
            } else {
                logging::log_info(&format!("backup: pruned {}", path.display()));
            }
        }
    }
}

fn today_stamp() -> String {
    chrono::Local::now().format("%Y%m%d").to_string()
}

/// Used by the "Open backups folder" button in the frontend.
pub fn backups_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("backups")
}
