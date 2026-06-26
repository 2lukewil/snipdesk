//! Paste telemetry endpoint. The desktop client tracks usage counts
//! locally (snipdesk-core/src/db.rs bump_usage); on each sync tick it
//! flushes accumulated deltas to this endpoint so the admin dashboard
//! can compute real "hours/money saved" figures from real activity
//! instead of the inventory-size proxy that shipped in 0004.
//!
//! Wire shape (POST /api/usage/report):
//!
//! ```json
//! {
//!   "chars_pasted_delta":    1234,
//!   "snippets_pasted_delta": 7,
//!   "personal": [
//!     {"id": "uuid-1", "delta": 2, "last_used": 1717000000},
//!     {"id": "uuid-2", "delta": 1, "last_used": 1717000100}
//!   ],
//!   "library": [
//!     {"id": "lib-uuid", "delta": 4, "last_used": 1717000100}
//!   ]
//! }
//! ```
//!
//! The server folds the deltas into:
//!   - `users.chars_pasted     += chars_pasted_delta`
//!   - `users.snippets_pasted  += snippets_pasted_delta`
//!   - `personal_snippets.usage_count += delta` (owner-scoped)
//!   - `library_usage` UPSERT per `(user_id, snippet_id)`
//!
//! The endpoint is intentionally idempotency-free in the
//! exactly-once sense - the client owns dedup via its local
//! "pending delta" rows, which are zeroed after a successful flush.
//! At-most-once delivery is fine for a metric (worst case we
//! under-count when a sync crashes mid-flight; we never double-count).

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::http::AppState;

#[derive(Debug, Deserialize)]
pub struct UsageReport {
    /// Total plaintext characters pasted since the last successful
    /// flush. May span multiple snippets.
    #[serde(default)]
    pub chars_pasted_delta: i64,
    /// Number of paste events since last flush (equals
    /// sum(personal.delta) + sum(library.delta) in practice, but
    /// the client sends it explicitly so the server doesn't have
    /// to reassemble it).
    #[serde(default)]
    pub snippets_pasted_delta: i64,
    #[serde(default)]
    pub personal: Vec<SnippetDelta>,
    #[serde(default)]
    pub library: Vec<SnippetDelta>,
}

#[derive(Debug, Deserialize)]
pub struct SnippetDelta {
    pub id: String,
    pub delta: i64,
    /// Unix-seconds timestamp of the most recent paste in this
    /// delta window. Used to populate `last_used`.
    pub last_used: i64,
}

/// `POST /api/usage/report`. Auth-required; folds deltas into the
/// per-user totals and per-snippet counters. Returns 204 on success.
pub async fn report(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<UsageReport>,
) -> Result<StatusCode, ApiError> {
    let user_id = &auth.0.sub;

    // Cheap sanity bounds. The desktop client can't possibly produce
    // these in a normal flush window; clamping prevents a buggy
    // client from blowing the totals out of proportion.
    if body.chars_pasted_delta < 0
        || body.snippets_pasted_delta < 0
        || body.chars_pasted_delta > 10_000_000
        || body.snippets_pasted_delta > 1_000_000
    {
        return Err(ApiError::bad_request("invalid_delta", "delta out of range"));
    }
    for d in body.personal.iter().chain(body.library.iter()) {
        if d.delta < 0 || d.delta > 1_000_000 {
            return Err(ApiError::bad_request(
                "invalid_delta",
                "snippet delta out of range",
            ));
        }
    }

    // One transaction so a partial failure rolls everything back -
    // we never want to bump `users.chars_pasted` without also
    // bumping the per-snippet counters that explain it.
    let mut tx = crate::db::begin_write(&state.pool)
        .await
        .map_err(|e| ApiError::internal(format!("usage report begin: {e}")))?;

    if body.chars_pasted_delta > 0 || body.snippets_pasted_delta > 0 {
        sqlx::query(
            "UPDATE users \
             SET chars_pasted = chars_pasted + ?1, \
                 snippets_pasted = snippets_pasted + ?2 \
             WHERE id = ?3",
        )
        .bind(body.chars_pasted_delta)
        .bind(body.snippets_pasted_delta)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::internal(format!("users update: {e}")))?;
    }

    for d in &body.personal {
        if d.delta == 0 {
            continue;
        }
        // owner_id gate keeps users from bumping each other's
        // counters - if a snippet id doesn't belong to this user
        // the UPDATE matches zero rows and silently no-ops, which
        // is the right behaviour for a "fire and forget" telemetry
        // flush.
        sqlx::query(
            "UPDATE personal_snippets \
             SET usage_count = usage_count + ?1, \
                 last_used = MAX(COALESCE(last_used, 0), ?2) \
             WHERE id = ?3 AND owner_id = ?4 AND is_deleted = 0",
        )
        .bind(d.delta)
        .bind(d.last_used)
        .bind(&d.id)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::internal(format!("personal usage: {e}")))?;
    }

    for d in &body.library {
        if d.delta == 0 {
            continue;
        }
        sqlx::query(
            "INSERT INTO library_usage (user_id, snippet_id, usage_count, last_used) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(user_id, snippet_id) DO UPDATE SET \
               usage_count = usage_count + excluded.usage_count, \
               last_used = MAX(library_usage.last_used, excluded.last_used)",
        )
        .bind(user_id)
        .bind(&d.id)
        .bind(d.delta)
        .bind(d.last_used)
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::internal(format!("library usage: {e}")))?;
    }

    tx.commit()
        .await
        .map_err(|e| ApiError::internal(format!("usage report commit: {e}")))?;

    Ok(StatusCode::NO_CONTENT)
}
