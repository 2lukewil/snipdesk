//! Shared team library: admin-managed, plaintext, visible to every
//! signed-in member.
//!
//! Differs from personal_snippets in three important ways:
//!
//!   1. **Plaintext** at rest. Library snippets are explicitly shared
//!      content (canned replies everyone uses); encrypting them buys
//!      nothing because every authenticated member needs to read them.
//!   2. **Org-wide version counter** rather than per-owner. A single
//!      monotonic stream means any signed-in client can pull "what
//!      changed since I last synced" with one `since` cursor.
//!   3. **Role-gated writes.** Any signed-in user can GET the library;
//!      only admins can POST/PUT/DELETE. Enforced server-side; the
//!      client UI also hides write buttons for non-admins, but the gate
//!      that matters is here.
//!
//! Sync shape mirrors personal_snippets: incremental list returns
//! tombstones (`is_deleted = true`) so a client deletion the user
//! made elsewhere lands on every other member's machine.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::http::AppState;

/// User-supplied fields. `tags` is a flat list (the wire shape used by
/// the desktop client); the DB stores it comma-padded for cheap LIKE
/// matching, same as personal_snippets and the local cache.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LibraryPayload {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub folder_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    /// Client-generated UUID. Admin tools (dashboard, CLI) own the id so
    /// they can retry idempotently if a network call fails mid-create.
    pub id: String,
    #[serde(flatten)]
    pub payload: LibraryPayload,
}

#[derive(Debug, Deserialize)]
pub struct UpdateBody {
    pub expected_version: i64,
    #[serde(flatten)]
    pub payload: LibraryPayload,
}

#[derive(Debug, Serialize)]
pub struct WriteResponse {
    pub id: String,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One row from the sync stream. `payload` is None for tombstones.
#[derive(Debug, Serialize)]
pub struct LibraryView {
    pub id: String,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub is_deleted: bool,
    pub payload: Option<LibraryPayload>,
}

#[derive(Debug, Serialize)]
pub struct SyncResponse {
    pub snippets: Vec<LibraryView>,
    pub high_water_mark: i64,
}

#[derive(Debug, Deserialize, Default)]
pub struct SyncQuery {
    #[serde(default)]
    pub since: Option<i64>,
}

/// Single global version counter. The library is org-wide so every
/// client tracks the same monotonic clock. Allocated inside the caller's
/// transaction so a flurry of admin edits stays serialized.
async fn next_version(tx: &mut sqlx::SqliteConnection) -> Result<i64, ApiError> {
    let (max,): (i64,) = sqlx::query_as("SELECT COALESCE(MAX(version), 0) FROM library_snippets")
        .fetch_one(&mut *tx)
        .await?;
    Ok(max + 1)
}

fn encode_tags(tags: &[String]) -> String {
    // Match the desktop client's storage shape so LIKE '%,tag,%'
    // matches both sides. Empty list → empty string (no leading comma)
    // because the client treats both as "no tags."
    if tags.is_empty() {
        return String::new();
    }
    let mut s = String::from(",");
    for t in tags {
        let t = t.trim().to_lowercase();
        if !t.is_empty() {
            s.push_str(&t);
            s.push(',');
        }
    }
    s
}

fn decode_tags(s: &str) -> Vec<String> {
    s.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

pub async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(q): Query<SyncQuery>,
) -> Result<Json<SyncResponse>, ApiError> {
    let since = q.since.unwrap_or(0);

    let rows: Vec<LibraryRow> = sqlx::query_as(
        "SELECT id, title, body, tags, folder_path, version, created_at, updated_at, is_deleted \
         FROM library_snippets \
         WHERE version > ? \
         ORDER BY version ASC",
    )
    .bind(since)
    .fetch_all(&state.pool)
    .await?;

    // Diagnostic: helps when a desktop client reports "shared library
    // snippets aren't showing up". If this line says `returned=0` while
    // the dashboard's Library tab shows N cards, the client's cursor
    // is past everything the server has - look at the desktop's
    // library_high_water_mark sync_state row.
    tracing::info!(
        caller = %auth.0.sub,
        since,
        returned = rows.len(),
        "library list"
    );

    let mut high_water_mark = since;
    let snippets = rows
        .into_iter()
        .map(|row| {
            if row.version > high_water_mark {
                high_water_mark = row.version;
            }
            let payload = if row.is_deleted != 0 {
                None
            } else {
                Some(LibraryPayload {
                    title: row.title,
                    body: row.body,
                    tags: decode_tags(&row.tags),
                    folder_path: row.folder_path,
                })
            };
            LibraryView {
                id: row.id,
                version: row.version,
                created_at: row.created_at,
                updated_at: row.updated_at,
                is_deleted: row.is_deleted != 0,
                payload,
            }
        })
        .collect();

    Ok(Json(SyncResponse {
        snippets,
        high_water_mark,
    }))
}

pub async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateBody>,
) -> Result<(StatusCode, Json<WriteResponse>), ApiError> {
    auth.require_admin()?;

    let mut tx = state.pool.begin().await?;

    // Reject id collisions explicitly so admin tools get a clean 409
    // instead of a constraint-violation 500.
    let exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM library_snippets WHERE id = ?")
        .bind(&body.id)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_some() {
        return Err(ApiError::conflict(
            "id_taken",
            "a library snippet with that id already exists",
        ));
    }

    let version = next_version(&mut tx).await?;
    let now = Utc::now().timestamp();
    let tags = encode_tags(&body.payload.tags);

    sqlx::query(
        "INSERT INTO library_snippets \
         (id, title, body, tags, folder_path, created_by, created_at, updated_at, version, is_deleted) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0)",
    )
    .bind(&body.id)
    .bind(&body.payload.title)
    .bind(&body.payload.body)
    .bind(&tags)
    .bind(&body.payload.folder_path)
    .bind(&auth.0.sub)
    .bind(now)
    .bind(now)
    .bind(version)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    tracing::info!(
        library_id = %body.id,
        created_by = %auth.0.sub,
        version,
        "library_snippet created"
    );

    Ok((
        StatusCode::CREATED,
        Json(WriteResponse {
            id: body.id,
            version,
            created_at: now,
            updated_at: now,
        }),
    ))
}

pub async fn update(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> Result<Json<WriteResponse>, ApiError> {
    auth.require_admin()?;

    let mut tx = state.pool.begin().await?;

    let current: Option<(i64, i64, i64)> =
        sqlx::query_as("SELECT version, created_at, is_deleted FROM library_snippets WHERE id = ?")
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?;
    let (current_version, created_at, is_deleted) =
        current.ok_or_else(|| ApiError::bad_request("not_found", "library snippet not found"))?;

    if is_deleted != 0 {
        return Err(ApiError::bad_request(
            "is_deleted",
            "library snippet has been deleted; create a fresh one with a new id",
        ));
    }
    if current_version != body.expected_version {
        // Two admins edited concurrently; the caller refetches and
        // re-applies. Mirrors personal_snippets behavior so admin tools
        // can share conflict-handling code.
        return Err(ApiError::conflict(
            "version_conflict",
            "library snippet was modified by another admin; refetch and retry",
        ));
    }

    let new_version = next_version(&mut tx).await?;
    let now = Utc::now().timestamp();
    let tags = encode_tags(&body.payload.tags);

    sqlx::query(
        "UPDATE library_snippets \
         SET title = ?, body = ?, tags = ?, folder_path = ?, updated_at = ?, version = ? \
         WHERE id = ?",
    )
    .bind(&body.payload.title)
    .bind(&body.payload.body)
    .bind(&tags)
    .bind(&body.payload.folder_path)
    .bind(now)
    .bind(new_version)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(Json(WriteResponse {
        id,
        version: new_version,
        created_at,
        updated_at: now,
    }))
}

pub async fn delete(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.require_admin()?;

    let mut tx = state.pool.begin().await?;

    let current: Option<(i64,)> =
        sqlx::query_as("SELECT is_deleted FROM library_snippets WHERE id = ?")
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?;
    let (is_deleted,) =
        current.ok_or_else(|| ApiError::bad_request("not_found", "library snippet not found"))?;
    if is_deleted != 0 {
        // Idempotent: deleting an already-deleted library snippet is a
        // no-op success. Lets admin tools retry safely.
        return Ok(StatusCode::NO_CONTENT);
    }

    let new_version = next_version(&mut tx).await?;
    let now = Utc::now().timestamp();
    sqlx::query(
        "UPDATE library_snippets \
         SET is_deleted = 1, version = ?, updated_at = ? \
         WHERE id = ?",
    )
    .bind(new_version)
    .bind(now)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(sqlx::FromRow)]
struct LibraryRow {
    id: String,
    title: String,
    body: String,
    tags: String,
    folder_path: Option<String>,
    version: i64,
    created_at: i64,
    updated_at: i64,
    is_deleted: i64,
}
