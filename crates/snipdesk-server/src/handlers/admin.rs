//! Admin endpoints for user management — list, role swap, disable.
//!
//! Every handler here is gated on `auth.require_admin()`; the dashboard
//! also locks its routes behind an admin-only cookie session, so reaching
//! these by accident from a non-admin client takes both a stolen JWT
//! AND knowledge of the API shape. Defence in depth.
//!
//! Snippet content is deliberately NOT exposed here — even admin views
//! get counts + timestamps + role metadata. The encrypt-at-rest design
//! relies on personal-snippet bodies never leaving their owner's API
//! surface; revealing them via /api/admin/users would break that.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::http::AppState;

/// One row in the admin users list. `snippet_count` is computed live
/// against personal_snippets (excluding tombstones) — the user_activity
/// pre-aggregation table was scoped for v1.1 but isn't wired yet; this
/// query is fine for tens of thousands of users on the SQLite backend.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AdminUserView {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub role: String,
    pub is_disabled: bool,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    pub snippet_count: i64,
}

pub async fn list_users(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<AdminUserView>>, ApiError> {
    auth.require_admin()?;

    // LEFT JOIN so users with zero snippets still appear. Tombstones
    // (is_deleted=1) don't count — they exist solely to propagate
    // deletes to client devices and would mislead an admin reading
    // "this user has 12 snippets" when they actually have 3.
    let rows: Vec<AdminUserRow> = sqlx::query_as(
        "SELECT u.id, u.email, u.display_name, u.role, u.is_disabled, \
                u.created_at, u.last_seen_at, \
                COALESCE(SUM(CASE WHEN s.is_deleted = 0 THEN 1 ELSE 0 END), 0) AS snippet_count \
         FROM users u \
         LEFT JOIN personal_snippets s ON s.owner_id = u.id \
         GROUP BY u.id \
         ORDER BY u.created_at ASC",
    )
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| AdminUserView {
                id: r.id,
                email: r.email,
                display_name: r.display_name,
                role: r.role,
                is_disabled: r.is_disabled != 0,
                created_at: r.created_at,
                last_seen_at: r.last_seen_at,
                snippet_count: r.snippet_count,
            })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
pub struct UpdateUserBody {
    /// Optional role swap. Accepts `"admin"` or `"member"` — any other
    /// value is rejected with a 400 so we can't accidentally introduce
    /// an unsanctioned role through a typo.
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub is_disabled: Option<bool>,
}

pub async fn update_user(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<UpdateUserBody>,
) -> Result<Json<AdminUserView>, ApiError> {
    auth.require_admin()?;

    if body.role.is_none() && body.is_disabled.is_none() {
        return Err(ApiError::bad_request(
            "no_changes",
            "at least one of role or is_disabled must be provided",
        ));
    }

    if let Some(r) = &body.role {
        if r != "admin" && r != "member" {
            return Err(ApiError::bad_request(
                "invalid_role",
                "role must be 'admin' or 'member'",
            ));
        }
    }

    // Self-protection: an admin can't lock themselves out by disabling
    // their own account or demoting themselves to member. The dashboard
    // also hides these buttons for the current row, but the server is
    // the gate that matters. Without this, one admin demoting themselves
    // could leave an org with zero admins and no way back.
    if id == auth.0.sub {
        if body.is_disabled == Some(true) {
            return Err(ApiError::bad_request(
                "self_disable",
                "you can't disable your own account",
            ));
        }
        if body.role.as_deref() == Some("member") {
            return Err(ApiError::bad_request(
                "self_demote",
                "you can't demote your own account",
            ));
        }
    }

    let mut tx = state.pool.begin().await?;
    let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM users WHERE id = ?")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_none() {
        return Err(ApiError::bad_request("not_found", "user not found"));
    }

    if let Some(role) = &body.role {
        // Prevent demoting the last remaining admin. If the caller is
        // demoting someone other than themselves and they're the only
        // other admin, we'd be leaving zero admins. Block it.
        if role == "member" {
            let admin_count: (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM users WHERE role = 'admin'")
                    .fetch_one(&mut *tx)
                    .await?;
            let target_is_admin: Option<(String,)> =
                sqlx::query_as("SELECT role FROM users WHERE id = ?")
                    .bind(&id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if target_is_admin.map(|r| r.0) == Some("admin".to_string()) && admin_count.0 <= 1 {
                return Err(ApiError::bad_request(
                    "last_admin",
                    "can't demote the last admin",
                ));
            }
        }
        sqlx::query("UPDATE users SET role = ? WHERE id = ?")
            .bind(role)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(disabled) = body.is_disabled {
        sqlx::query("UPDATE users SET is_disabled = ? WHERE id = ?")
            .bind(if disabled { 1 } else { 0 })
            .bind(&id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    let row: AdminUserRow = sqlx::query_as(
        "SELECT u.id, u.email, u.display_name, u.role, u.is_disabled, \
                u.created_at, u.last_seen_at, \
                COALESCE(SUM(CASE WHEN s.is_deleted = 0 THEN 1 ELSE 0 END), 0) AS snippet_count \
         FROM users u \
         LEFT JOIN personal_snippets s ON s.owner_id = u.id \
         WHERE u.id = ? \
         GROUP BY u.id",
    )
    .bind(&id)
    .fetch_one(&state.pool)
    .await?;

    Ok(Json(AdminUserView {
        id: row.id,
        email: row.email,
        display_name: row.display_name,
        role: row.role,
        is_disabled: row.is_disabled != 0,
        created_at: row.created_at,
        last_seen_at: row.last_seen_at,
        snippet_count: row.snippet_count,
    }))
}

pub async fn delete_user(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.require_admin()?;

    if id == auth.0.sub {
        return Err(ApiError::bad_request(
            "self_delete",
            "you can't delete your own account",
        ));
    }

    // Hard delete cascades to personal_snippets via the FK ON DELETE
    // CASCADE in 0001. Tombstones aren't necessary because no client
    // syncs as this user any more — when they next sign in (they can't)
    // they would get nothing back.
    let res = sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::bad_request("not_found", "user not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(sqlx::FromRow)]
struct AdminUserRow {
    id: String,
    email: String,
    display_name: String,
    role: String,
    is_disabled: i64,
    created_at: i64,
    last_seen_at: Option<i64>,
    snippet_count: i64,
}
