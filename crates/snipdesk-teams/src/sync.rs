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
//! PUT with stale `expected_version`) is silently dropped - the
//! subsequent pull will overwrite the local row with the server's
//! version. This loses the user's concurrent edit. The design doc
//! upgrades this to "preserve loser as a `(conflict YYYY-MM-DD)`
//! snippet" in v1.1; the v1 protocol is forward-compatible with that.
//!
//! Errors at any individual step are logged and the tick continues -
//! we don't want one bad snippet to wedge the whole engine.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use snipdesk_core::db::{Db, TelemetryKind};

use crate::api::{
    self, ApiError, CreateBody, SnippetPayload, UpdateBody, UsageReport, UsageSnippetDelta,
};

/// Aggregate result of one `tick()` call. Surfaced via the
/// `server_status` IPC command so the settings UI can show "last sync N
/// seconds ago, X pushed, Y pulled."
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SyncOutcome {
    pub pushed: usize,
    pub pulled: usize,
    pub deleted_remote: usize,
    pub applied_local_deletes: usize,
    /// Library rows refreshed or freshly inserted into team_snippets
    /// during this tick. Surfaced for parity with `pulled` so the UI can
    /// report "library: N changes" alongside personal-snippet stats.
    #[serde(default)]
    pub library_pulled: usize,
    /// Library rows the server tombstoned that we dropped locally.
    #[serde(default)]
    pub library_deleted: usize,
    /// Best-effort error count. A network blip during one snippet
    /// doesn't fail the tick; we just count it and move on.
    pub errors: usize,
    /// When the tick was performed (unix seconds).
    pub at: i64,
    /// The user record as the server sees it right now. Populated by
    /// the GET /api/me probe at the start of each tick; surfaced so
    /// the IPC layer can detect "my role just changed" and re-render
    /// the identity panel without waiting for the user to sign out
    /// and back in. None when the probe itself didn't run (e.g.
    /// network failure) - the caller should treat that as "use the
    /// previous value" rather than "user vanished".
    #[serde(default)]
    pub user: Option<api::UserDto>,
    /// When the server rotates our token (current one is near
    /// expiry), the new one lands here. The IPC layer swaps the
    /// keychain entry; subsequent ticks pick up the fresh token via
    /// the normal credentials::load path. None on the typical case
    /// where rotation wasn't needed yet.
    #[serde(default)]
    pub refreshed_token: Option<String>,
}

/// One full sync round. Takes a locked Db so the caller (Tauri
/// background thread) controls when the SQLite lock is held.
pub fn tick(db: &Mutex<Db>, server_url: &str, token: &str) -> Result<SyncOutcome, ApiError> {
    let mut out = SyncOutcome {
        at: chrono::Utc::now().timestamp(),
        ..SyncOutcome::default()
    };

    // 0. Probe /api/me first. This is what makes role changes and
    //    account-disable propagate to the running desktop client
    //    without the user having to sign out and back in. The probe
    //    also hits the AuthUser extractor on the server, which is the
    //    enforcement point for is_disabled / account_gone, so any
    //    "your account is no longer valid" condition bubbles up here
    //    rather than later in the tick. Network failures are tolerated
    //    (None in out.user means "I couldn't check this round").
    match api::me(server_url, token) {
        Ok(me) => {
            out.user = Some(me.user);
            // Token rotation: the server returns `refreshed_token`
            // when our current one is nearing expiry. Forward it so
            // the IPC layer can persist the new token; subsequent
            // ticks pick it up from the keychain via the normal
            // credentials::load path. The CURRENT tick still uses
            // the old token for the rest of its requests - that's
            // fine because the old token is, by definition, not
            // expired yet.
            if let Some(t) = me.refreshed_token {
                out.refreshed_token = Some(t);
            }
        }
        Err(ApiError::Unauthorized) => return Err(ApiError::Unauthorized),
        Err(ApiError::AccountInactive(msg)) => return Err(ApiError::AccountInactive(msg)),
        Err(e) => {
            eprintln!("me probe failed: {e}");
            out.errors += 1;
        }
    }

    // 1. Drain tombstones first. If a user deleted then re-created with
    //    the same id (impossible - UUIDs - but defensive), processing
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
            Err(ApiError::AccountInactive(msg)) => return Err(ApiError::AccountInactive(msg)),
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
            // Stale push: leave the row dirty=1 for now - step 3's pull
            // will overwrite it with the server's content and clear
            // dirty as part of upsert_from_remote. The local edit is
            // lost (v1 LWW); v1.1 will preserve it as a conflict copy.
            Err(ApiError::VersionConflict) => {
                eprintln!("push {} conflict; will reconcile via pull", d.id);
                out.errors += 1;
            }
            Err(ApiError::Unauthorized) => return Err(ApiError::Unauthorized),
            Err(ApiError::AccountInactive(msg)) => return Err(ApiError::AccountInactive(msg)),
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

    // 4. Pull the shared library since the last library-HWM. Tracked
    //    under its own sync_state key because the library has a
    //    separate version stream (org-wide rather than per-user) on the
    //    server; mixing the two cursors would either re-download
    //    library rows or skip personal updates depending on which was
    //    higher. The library is pull-only from the desktop client (admin
    //    writes happen in the dashboard), so this is a strict mirror
    //    into team_snippets.
    let library_since = {
        let db = db
            .lock()
            .map_err(|e| ApiError::Network(format!("db poisoned: {e}")))?;
        db.load_sync_state("library_high_water_mark")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
    };
    match api::list_library(server_url, token, library_since) {
        Ok(lib) => {
            for s in &lib.snippets {
                if s.is_deleted {
                    if let Ok(db) = db.lock() {
                        let _ = db.delete_library_snippet(&s.id);
                    }
                    out.library_deleted += 1;
                } else if let Some(p) = &s.payload {
                    if let Ok(db) = db.lock() {
                        if let Err(e) = db.upsert_library_snippet(
                            &s.id,
                            &p.title,
                            &p.body,
                            &p.tags,
                            p.folder_path.as_deref(),
                            s.version,
                        ) {
                            eprintln!("library upsert {} failed: {e}", s.id);
                            out.errors += 1;
                            continue;
                        }
                    }
                    out.library_pulled += 1;
                }
            }
            let lib_hwm = lib.high_water_mark.max(library_since);
            if let Ok(db) = db.lock() {
                let _ = db.save_sync_state("library_high_water_mark", &lib_hwm.to_string());
            }
        }
        Err(ApiError::Unauthorized) => return Err(ApiError::Unauthorized),
        Err(e) => {
            // Library is non-critical for v1 - a transient failure here
            // shouldn't sink the whole tick. Surface as an error count
            // and keep the personal-sync results.
            eprintln!("library pull failed: {e}");
            out.errors += 1;
        }
    }

    // 5. Flush paste telemetry. Snapshot the pending counters, post,
    //    and on success subtract the snapshot amounts from the live
    //    rows. Any user activity that happened between snapshot and
    //    commit accumulates and survives.
    //
    //    Strictly best-effort: a network blip here costs one batch of
    //    metric data and nothing more. We log + count an error and
    //    leave the snapshot on disk, where the next tick will retry.
    let snapshot = {
        let db = db
            .lock()
            .map_err(|e| ApiError::Network(format!("db poisoned: {e}")))?;
        db.snapshot_telemetry()
            .map_err(|e| ApiError::Network(format!("telemetry snapshot: {e}")))?
    };
    if !snapshot.is_empty() {
        let mut chars_total: i64 = 0;
        let mut snippets_total: i64 = 0;
        let mut personal: Vec<UsageSnippetDelta> = Vec::new();
        let mut library: Vec<UsageSnippetDelta> = Vec::new();
        for s in &snapshot {
            chars_total += s.chars;
            snippets_total += s.delta;
            let bucket = match s.kind {
                TelemetryKind::Personal => &mut personal,
                TelemetryKind::Library => &mut library,
            };
            bucket.push(UsageSnippetDelta {
                id: &s.snippet_id,
                delta: s.delta,
                last_used: s.last_used,
            });
        }
        let body = UsageReport {
            chars_pasted_delta: chars_total,
            snippets_pasted_delta: snippets_total,
            personal: &personal,
            library: &library,
        };
        match api::report_usage(server_url, token, &body) {
            Ok(()) => {
                if let Ok(db) = db.lock() {
                    if let Err(e) = db.commit_telemetry_flush(&snapshot) {
                        eprintln!("telemetry commit failed: {e}");
                        out.errors += 1;
                    }
                }
            }
            Err(ApiError::Unauthorized) => return Err(ApiError::Unauthorized),
            Err(ApiError::AccountInactive(msg)) => {
                return Err(ApiError::AccountInactive(msg));
            }
            Err(e) => {
                eprintln!("telemetry report failed: {e}");
                out.errors += 1;
            }
        }
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
    // post-condition matches one tick() of the engine, so reuse it -
    // a fresh-install user has every existing row dirty=1 with
    // server_version=NULL, so tick() will POST each one.
    let outcome = tick(db, server_url, token)?;
    Ok(outcome.pushed)
}
