//! Personal-snippet CRUD + incremental sync.
//!
//! Every row's user-provided content is encrypted at rest by the
//! `crypto` module before insert and decrypted on the way back out. The
//! server holds the master key (see `docs/server-design.md`); a DB dump
//! reveals only opaque ciphertext + metadata.
//!
//! Sync model:
//!   - Each user has their own monotonic `version` counter - the highest
//!     value across all of *their* personal_snippets rows. Every
//!     write (create/update/delete) gets `max(version) + 1`.
//!   - `GET /api/snippets?since=N` returns every row with version > N
//!     for the authenticated user, *including* tombstones (so the
//!     client deletes locally).
//!   - `PUT` carries `expected_version`; mismatch → 409 (client
//!     reconciles via a fresh GET).
//!   - `DELETE` is a soft delete: bump version, set is_deleted=1, leave
//!     the ciphertext as-is. The server never re-decrypts tombstones
//!     (the AD would mismatch the bumped version anyway - by design,
//!     since the client doesn't need the body of a deleted snippet).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::crypto::{decrypt_payload, encrypt_payload, EncryptedBlob, SnippetPayload};
use crate::error::ApiError;
use crate::http::AppState;

/// Current encryption-key generation. Recorded on every row so a future
/// rotation pass can find rows still encrypted under an older key. Bump
/// this constant when introducing key_version 2.
const CURRENT_KEY_VERSION: i64 = 1;

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    /// Client-generated UUID. The client owns this id from creation so
    /// it can sync without first round-tripping to the server. Server
    /// rejects collisions with 409.
    pub id: String,
    #[serde(flatten)]
    pub payload: SnippetPayload,
}

#[derive(Debug, Deserialize)]
pub struct UpdateBody {
    pub expected_version: i64,
    #[serde(flatten)]
    pub payload: SnippetPayload,
}

/// What we return from the writes (POST/PUT). The client already has
/// the payload it sent; we just confirm the server's view of versioning.
#[derive(Debug, Serialize)]
pub struct WriteResponse {
    pub id: String,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A snippet as it appears in the sync stream. `payload` is `None` for
/// tombstones (`is_deleted == true`); clients see the row and locally
/// drop the corresponding entry.
#[derive(Debug, Serialize)]
pub struct SnippetView {
    pub id: String,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub is_deleted: bool,
    pub payload: Option<SnippetPayload>,
}

#[derive(Debug, Serialize)]
pub struct SyncResponse {
    pub snippets: Vec<SnippetView>,
    /// The highest version returned in this batch. Clients use it as
    /// `since` on the next sync tick. Equal to the input `since` when
    /// the batch is empty (no progress).
    pub high_water_mark: i64,
}

#[derive(Debug, Deserialize, Default)]
pub struct SyncQuery {
    #[serde(default)]
    pub since: Option<i64>,
}

/// Compute the next monotonic version for this user. SQLite's WAL gives
/// us serializable writes under BEGIN IMMEDIATE; with one server in
/// front of one DB file this is race-free. (A multi-writer server would
/// need a stronger guarantee; phase 1 server is single-process.)
async fn next_version(tx: &mut sqlx::SqliteConnection, owner_id: &str) -> Result<i64, ApiError> {
    let (max,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(MAX(version), 0) FROM personal_snippets WHERE owner_id = ?",
    )
    .bind(owner_id)
    .fetch_one(&mut *tx)
    .await?;
    Ok(max + 1)
}

pub async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateBody>,
) -> Result<(StatusCode, Json<WriteResponse>), ApiError> {
    let owner_id = &auth.0.sub;

    let mut tx = state.pool.begin().await?;

    // Reject id collisions explicitly so the client gets a clear 409
    // instead of a SQLITE_CONSTRAINT 500 from the PRIMARY KEY. Checked
    // globally - a snippet id collision across users would be a
    // client-bug too (UUIDv4 collisions are astronomically unlikely),
    // and treating it as a global namespace is simpler.
    let exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM personal_snippets WHERE id = ?")
        .bind(&body.id)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_some() {
        return Err(ApiError::conflict(
            "id_taken",
            "a snippet with that id already exists",
        ));
    }

    let version = next_version(&mut tx, owner_id).await?;
    let blob = encrypt_payload(
        &state.master_key,
        &body.payload,
        &body.id,
        owner_id,
        version,
    )
    .map_err(|e| ApiError::internal(format!("encrypt: {e}")))?;
    let now = Utc::now().timestamp();

    let result = sqlx::query(
        "INSERT INTO personal_snippets \
         (id, owner_id, payload_ciphertext, payload_nonce, key_version, version, created_at, updated_at, is_deleted, encrypted_version) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, ?)",
    )
    .bind(&body.id)
    .bind(owner_id)
    .bind(&blob.ciphertext)
    .bind(&blob.nonce)
    .bind(CURRENT_KEY_VERSION)
    .bind(version)
    .bind(now)
    .bind(now)
    .bind(version)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    // Diagnostic log: prove the INSERT actually committed before we
    // return CREATED. If the client claims "N uploaded" but the admin
    // panel shows 0, this line in the server log tells us whether the
    // problem is on the write side (no log lines) or the read side
    // (log lines present but query disagrees).
    tracing::info!(
        snippet_id = %body.id,
        owner_id = %owner_id,
        version,
        rows_affected = result.rows_affected(),
        "personal_snippet created"
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

pub async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(q): Query<SyncQuery>,
) -> Result<Json<SyncResponse>, ApiError> {
    let owner_id = &auth.0.sub;
    let since = q.since.unwrap_or(0);

    let rows: Vec<SnippetRow> = sqlx::query_as(
        "SELECT id, payload_ciphertext, payload_nonce, version, created_at, updated_at, is_deleted \
         FROM personal_snippets \
         WHERE owner_id = ? AND version > ? \
         ORDER BY version ASC",
    )
    .bind(owner_id)
    .bind(since)
    .fetch_all(&state.pool)
    .await?;

    let mut snippets = Vec::with_capacity(rows.len());
    let mut high_water_mark = since;
    for row in rows {
        if row.version > high_water_mark {
            high_water_mark = row.version;
        }
        let payload = if row.is_deleted != 0 {
            None
        } else {
            // We trust the DB content (it came from us, encrypted with our
            // key); a decrypt failure here means corruption or key
            // mismatch, both 500-grade.
            let blob = EncryptedBlob {
                ciphertext: row.payload_ciphertext,
                nonce: row.payload_nonce,
            };
            Some(
                decrypt_payload(&state.master_key, &blob, &row.id, owner_id, row.version)
                    .map_err(|e| ApiError::internal(format!("decrypt {}: {e}", row.id)))?,
            )
        };
        snippets.push(SnippetView {
            id: row.id,
            version: row.version,
            created_at: row.created_at,
            updated_at: row.updated_at,
            is_deleted: row.is_deleted != 0,
            payload,
        });
    }

    Ok(Json(SyncResponse {
        snippets,
        high_water_mark,
    }))
}

pub async fn update(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> Result<Json<WriteResponse>, ApiError> {
    let owner_id = &auth.0.sub;

    let mut tx = state.pool.begin().await?;

    let current: Option<(i64, i64, i64)> = sqlx::query_as(
        "SELECT version, created_at, is_deleted FROM personal_snippets WHERE id = ? AND owner_id = ?",
    )
    .bind(&id)
    .bind(owner_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (current_version, created_at, is_deleted) =
        current.ok_or_else(|| ApiError::bad_request("not_found", "snippet not found"))?;

    // Updates on a tombstone are a client bug (already deleted) - say
    // so explicitly rather than silently resurrecting.
    if is_deleted != 0 {
        return Err(ApiError::bad_request(
            "is_deleted",
            "snippet has been deleted; create a fresh one with a new id",
        ));
    }

    if current_version != body.expected_version {
        // The client's view is stale. Don't expose the server's content
        // here (the client should re-fetch via GET); just signal the
        // conflict and let the client reconcile.
        return Err(ApiError::conflict(
            "version_conflict",
            "snippet was modified by another client; refetch and retry",
        ));
    }

    let new_version = next_version(&mut tx, owner_id).await?;
    let blob = encrypt_payload(&state.master_key, &body.payload, &id, owner_id, new_version)
        .map_err(|e| ApiError::internal(format!("encrypt: {e}")))?;
    let now = Utc::now().timestamp();
    sqlx::query(
        "UPDATE personal_snippets \
         SET payload_ciphertext = ?, payload_nonce = ?, key_version = ?, version = ?, updated_at = ?, encrypted_version = ? \
         WHERE id = ? AND owner_id = ?",
    )
    .bind(&blob.ciphertext)
    .bind(&blob.nonce)
    .bind(CURRENT_KEY_VERSION)
    .bind(new_version)
    .bind(now)
    .bind(new_version)
    .bind(&id)
    .bind(owner_id)
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
    let owner_id = &auth.0.sub;
    let mut tx = state.pool.begin().await?;

    let current: Option<(i64,)> =
        sqlx::query_as("SELECT is_deleted FROM personal_snippets WHERE id = ? AND owner_id = ?")
            .bind(&id)
            .bind(owner_id)
            .fetch_optional(&mut *tx)
            .await?;
    let (is_deleted,) =
        current.ok_or_else(|| ApiError::bad_request("not_found", "snippet not found"))?;
    if is_deleted != 0 {
        // Idempotent: deleting an already-deleted snippet is a no-op
        // success. Lets clients retry safely without surfacing errors.
        return Ok(StatusCode::NO_CONTENT);
    }

    let new_version = next_version(&mut tx, owner_id).await?;
    let now = Utc::now().timestamp();
    // Tombstone: bump version + flag deleted. We deliberately do NOT
    // touch payload_ciphertext / payload_nonce - the server never
    // decrypts a tombstone (AD mismatch would fail anyway), so the
    // stale ciphertext is inert.
    sqlx::query(
        "UPDATE personal_snippets \
         SET is_deleted = 1, version = ?, updated_at = ? \
         WHERE id = ? AND owner_id = ?",
    )
    .bind(new_version)
    .bind(now)
    .bind(&id)
    .bind(owner_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(StatusCode::NO_CONTENT)
}

/// List soft-deleted snippets for the calling user, with decrypted
/// payloads, so the desktop client can show a "Trash" view. Distinct
/// from `GET /api/snippets` because that endpoint deliberately
/// returns tombstones with `payload: null` for sync purposes - the
/// trash view needs the actual contents so the user can decide what
/// to restore. Sorted by `updated_at DESC` (most-recently-deleted
/// first) which matches how a user thinks about trash.
pub async fn trash(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<TrashView>>, ApiError> {
    let owner_id = &auth.0.sub;

    let rows: Vec<TrashRow> = sqlx::query_as(
        "SELECT id, payload_ciphertext, payload_nonce, version, encrypted_version, \
                created_at, updated_at \
         FROM personal_snippets \
         WHERE owner_id = ? AND is_deleted = 1 \
         ORDER BY updated_at DESC",
    )
    .bind(owner_id)
    .fetch_all(&state.pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        // Tombstoned rows weren't re-encrypted at delete time - the
        // ciphertext is still bound to whatever version the row was
        // at just before delete. That value lives in
        // encrypted_version (added in 0003); fall back to version-1
        // for rows that predate the column population, which the
        // migration already corrected but we keep the safety net.
        let ad_version = row.encrypted_version.unwrap_or(row.version - 1);
        let blob = EncryptedBlob {
            ciphertext: row.payload_ciphertext,
            nonce: row.payload_nonce,
        };
        let payload = match decrypt_payload(&state.master_key, &blob, &row.id, owner_id, ad_version)
        {
            Ok(p) => p,
            Err(e) => {
                // A decrypt failure here is non-fatal - we just skip
                // that row from the trash list. A user with one
                // corrupt tombstone shouldn't lose visibility on the
                // rest of their trash. Log so an operator can spot
                // it.
                tracing::warn!(
                    snippet_id = %row.id,
                    error = %e,
                    "trash decrypt failed; skipping"
                );
                continue;
            }
        };
        out.push(TrashView {
            id: row.id,
            version: row.version,
            created_at: row.created_at,
            deleted_at: row.updated_at,
            payload,
        });
    }
    Ok(Json(out))
}

/// Un-delete a snippet from the trash. Bumps version (so other
/// devices learn about the resurrection on their next pull), flips
/// is_deleted back to 0, and re-encrypts the payload under the new
/// version so the AD binding stays consistent.
pub async fn restore(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<WriteResponse>, ApiError> {
    let owner_id = &auth.0.sub;

    let mut tx = state.pool.begin().await?;

    let current: Option<RestoreRow> = sqlx::query_as(
        "SELECT id, payload_ciphertext, payload_nonce, version, encrypted_version, created_at, is_deleted \
         FROM personal_snippets WHERE id = ? AND owner_id = ?",
    )
    .bind(&id)
    .bind(owner_id)
    .fetch_optional(&mut *tx)
    .await?;
    let row = current.ok_or_else(|| ApiError::bad_request("not_found", "snippet not found"))?;

    if row.is_deleted == 0 {
        // Not in the trash; nothing to restore. Treat as a noop
        // success so the client can be idempotent.
        return Ok(Json(WriteResponse {
            id,
            version: row.version,
            created_at: row.created_at,
            updated_at: Utc::now().timestamp(),
        }));
    }

    // Decrypt with the version the ciphertext was bound to.
    let ad_version = row.encrypted_version.unwrap_or(row.version - 1);
    let blob = EncryptedBlob {
        ciphertext: row.payload_ciphertext,
        nonce: row.payload_nonce,
    };
    let payload = decrypt_payload(&state.master_key, &blob, &id, owner_id, ad_version)
        .map_err(|e| ApiError::internal(format!("decrypt during restore: {e}")))?;

    // Re-encrypt under the new version so the AD binding stays in
    // lockstep with the row's version.
    let new_version = next_version(&mut tx, owner_id).await?;
    let new_blob = encrypt_payload(&state.master_key, &payload, &id, owner_id, new_version)
        .map_err(|e| ApiError::internal(format!("encrypt during restore: {e}")))?;
    let now = Utc::now().timestamp();

    sqlx::query(
        "UPDATE personal_snippets \
         SET payload_ciphertext = ?, payload_nonce = ?, key_version = ?, \
             version = ?, updated_at = ?, encrypted_version = ?, is_deleted = 0 \
         WHERE id = ? AND owner_id = ?",
    )
    .bind(&new_blob.ciphertext)
    .bind(&new_blob.nonce)
    .bind(CURRENT_KEY_VERSION)
    .bind(new_version)
    .bind(now)
    .bind(new_version)
    .bind(&id)
    .bind(owner_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    tracing::info!(snippet_id = %id, owner_id = %owner_id, version = new_version, "personal_snippet restored");

    Ok(Json(WriteResponse {
        id,
        version: new_version,
        created_at: row.created_at,
        updated_at: now,
    }))
}

#[derive(Debug, Serialize)]
pub struct TrashView {
    pub id: String,
    pub version: i64,
    pub created_at: i64,
    /// When the user deleted it (== updated_at on the row, since the
    /// delete handler bumps updated_at at tombstone time).
    pub deleted_at: i64,
    pub payload: SnippetPayload,
}

#[derive(sqlx::FromRow)]
struct TrashRow {
    id: String,
    payload_ciphertext: Vec<u8>,
    payload_nonce: Vec<u8>,
    version: i64,
    encrypted_version: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

#[derive(sqlx::FromRow)]
struct RestoreRow {
    #[allow(dead_code)] // id is in the URL path; this is just the row's mirror
    id: String,
    payload_ciphertext: Vec<u8>,
    payload_nonce: Vec<u8>,
    version: i64,
    encrypted_version: Option<i64>,
    created_at: i64,
    is_deleted: i64,
}

#[derive(sqlx::FromRow)]
struct SnippetRow {
    id: String,
    payload_ciphertext: Vec<u8>,
    payload_nonce: Vec<u8>,
    version: i64,
    created_at: i64,
    updated_at: i64,
    is_deleted: i64,
}
