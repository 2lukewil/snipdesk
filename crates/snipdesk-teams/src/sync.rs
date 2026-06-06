//! Two-way sync engine for personal snippets.
//!
//! One `tick()` does:
//!   1. Drain `pending_deletes` → `DELETE /api/snippets/:id` for each.
//!   2. Push every locally-dirty row (POST if never-pushed, PUT
//!      otherwise with `expected_version` for optimistic concurrency).
//!   3. Pull `GET /api/snippets?since=<high_water_mark>` and apply the
//!      response (upsert/delete) locally.
//!   4. Save the new high-water mark for the next tick.
//!
//! Conflict policy for v1: **last-write-wins, server is the source of
//! truth.** A push that comes back with `version_conflict` (we tried to
//! PUT with stale `expected_version`) is silently dropped — the
//! subsequent pull will overwrite the local row with the server's
//! version. This loses the user's concurrent edit. The design doc
//! upgrades this to "preserve loser as a `(conflict YYYY-MM-DD)`
//! snippet" in v1.1; the v1 protocol is forward-compatible with that.
//!
//! Errors at any individual step are logged and the tick continues —
//! we don't want one bad snippet to wedge the whole engine.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use snipdesk_core::db::Db;

use crate::api::{self, ApiError, CreateBody, SnippetPayload, UpdateBody};

/// Aggregate result of one `tick()` call. Surfaced via the
/// `server_status` IPC command so the settings UI can show "last sync N
/// seconds ago, X pushed, Y pulled."
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SyncOutcome {
    pub pushed: usize,
    pub pulled: usize,
    pub deleted_remote: usize,
    pub applied_local_deletes: usize,
    /// Best-effort error count. A network blip during one snippet
    /// doesn't fail the tick; we just count it and move on.
    pub errors: usize,
    /// When the tick was performed (unix seconds).
    pub at: i64,
}

/// One full sync round. Takes a locked Db so the caller (Tauri
/// background thread) controls when the SQLite lock is held.
pub fn tick(db: &Mutex<Db>, server_url: &str, token: &str) -> Result<SyncOutcome, ApiError> {
    let mut out = SyncOutcome {
        at: chrono::Utc::now().timestamp(),
        ..SyncOutcome::default()
    };

    // 1. Drain tombstones first. If a user deleted then re-created with
    //    the same id (impossible — UUIDs — but defensive), processing
    //    deletes first means the recreate always wins.
    let tombstones = {
        let db = db
            .lock()
            .map_err(|e| ApiError::Network(format!("db poisoned: {e}")))?;
        db.pending_deletes()
            .map_err(|e| ApiError::Network(format!("db: {e}")))?
    };
    for id in tombstones {
        match api::delete_snippet(server_url, token, &id) {
            Ok(()) => {
                if let Ok(db) = db.lock() {
                    let _ = db.clear_pending_delete(&id);
                }
                out.deleted_remote += 1;
            }
            // 400 "not_found" → server already lacks it; same end-state,
            // so clear the local tombstone.
            Err(ApiError::Server { code, .. }) if code == "not_found" => {
                if let Ok(db) = db.lock() {
                    let _ = db.clear_pending_delete(&id);
                }
                out.deleted_remote += 1;
            }
            Err(ApiError::Unauthorized) => return Err(ApiError::Unauthorized),
            Err(e) => {
                eprintln!("delete {id} failed: {e}");
                out.errors += 1;
            }
        }
    }

    // 2. Push every dirty row. We refetch the list inside this
    //    scope so it picks up any concurrent edits made since we
    //    started the tick.
    let dirty = {
        let db = db
            .lock()
            .map_err(|e| ApiError::Network(format!("db poisoned: {e}")))?;
        db.dirty_snippets()
            .map_err(|e| ApiError::Network(format!("db: {e}")))?
    };
    let mut max_pushed_version = 0i64;
    for d in dirty {
        let payload = SnippetPayload {
            title: d.title,
            body: d.body,
            tags: d.tags,
            folder_path: d.folder_path,
        };
        let result = match d.server_version {
            None => api::create_snippet(
                server_url,
                token,
                &CreateBody {
                    id: &d.id,
                    payload: &payload,
                },
            ),
            Some(prev) => api::update_snippet(
                server_url,
                token,
                &d.id,
                &UpdateBody {
                    expected_version: prev,
                    payload: &payload,
                },
            ),
        };
        match result {
            Ok(resp) => {
                if let Ok(db) = db.lock() {
                    let _ = db.mark_synced(&d.id, resp.version);
                }
                max_pushed_version = max_pushed_version.max(resp.version);
                out.pushed += 1;
            }
            // Stale push: leave the row dirty=1 for now — step 3's pull
            // will overwrite it with the server's content and clear
            // dirty as part of upsert_from_remote. The local edit is
            // lost (v1 LWW); v1.1 will preserve it as a conflict copy.
            Err(ApiError::VersionConflict) => {
                eprintln!("push {} conflict; will reconcile via pull", d.id);
                out.errors += 1;
            }
            Err(ApiError::Unauthorized) => return Err(ApiError::Unauthorized),
            Err(e) => {
                eprintln!("push {} failed: {e}", d.id);
                out.errors += 1;
            }
        }
    }

    // 3. Pull remote changes since the last seen high-water mark. If we
    //    just pushed something at version N, advance the cursor past it
    //    so the response doesn't redundantly include our own write.
    let stored_hwm = {
        let db = db
            .lock()
            .map_err(|e| ApiError::Network(format!("db poisoned: {e}")))?;
        db.load_sync_state("high_water_mark")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
    };
    let pull_since = stored_hwm.max(max_pushed_version);
    let resp = api::list_snippets(server_url, token, pull_since)?;
    for s in &resp.snippets {
        if s.is_deleted {
            if let Ok(db) = db.lock() {
                let _ = db.apply_remote_delete(&s.id);
            }
            out.applied_local_deletes += 1;
        } else if let Some(p) = &s.payload {
            if let Ok(db) = db.lock() {
                if let Err(e) = db.upsert_from_remote(
                    &s.id,
                    &p.title,
                    &p.body,
                    &p.tags,
                    p.folder_path.as_deref(),
                    s.version,
                    s.created_at,
                    s.updated_at,
                ) {
                    eprintln!("upsert {} failed: {e}", s.id);
                    out.errors += 1;
                    continue;
                }
            }
            out.pulled += 1;
        }
    }
    let new_hwm = resp.high_water_mark.max(pull_since);
    if let Ok(db) = db.lock() {
        let _ = db.save_sync_state("high_water_mark", &new_hwm.to_string());
    }

    Ok(out)
}

/// Upload every existing local snippet to a fresh server account. Called
/// from the first-login flow when the user opts to migrate their
/// existing Lite library. Idempotent: rows that already have a
/// `server_version` are skipped (the regular sync handles them).
pub fn migrate_existing_local(
    db: &Mutex<Db>,
    server_url: &str,
    token: &str,
) -> Result<usize, ApiError> {
    // We just enumerate ALL dirty rows with server_version=NULL. The
    // post-condition matches one tick() of the engine, so reuse it —
    // a fresh-install user has every existing row dirty=1 with
    // server_version=NULL, so tick() will POST each one.
    let outcome = tick(db, server_url, token)?;
    Ok(outcome.pushed)
}
