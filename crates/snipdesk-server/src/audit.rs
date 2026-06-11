//! Structured audit log for admin mutations.
//!
//! Backs the `audit_log` table from migration 0007. Every admin
//! mutation in the dashboard or admin JSON API funnels through
//! `record()` so an operator can later answer "who promoted Bob"
//! without grepping shipped logs.
//!
//! Append-only by design: no UPDATE or DELETE helpers live here. If
//! retention ever becomes a problem the operator can prune the
//! table out-of-band; the app side won't notice.
//!
//! Errors from the insert are logged and swallowed - we don't want
//! a write to the audit table to fail an otherwise-successful
//! admin action (the mutation would already have committed by the
//! time audit runs in the typical "do work, then record" flow).

use serde_json::Value as JsonValue;
use sqlx::SqlitePool;

/// Dotted action codes. Centralising them here keeps the
/// dashboard's "recent activity" view safe from typos and lets a
/// future grep find every site that records each kind. Add new
/// variants here as new admin mutations land.
pub mod action {
    pub const USER_CREATE: &str = "user.create";
    pub const USER_UPDATE: &str = "user.update";
    pub const USER_DELETE: &str = "user.delete";
    pub const LIBRARY_CREATE: &str = "library.create";
    pub const LIBRARY_UPDATE: &str = "library.update";
    pub const LIBRARY_DELETE: &str = "library.delete";
    pub const LIBRARY_EXPORT: &str = "library.export";
    pub const LIBRARY_IMPORT: &str = "library.import";
    // A drag-drop move from the dashboard goes through library::update
    // under the hood, so it lands as LIBRARY_UPDATE with the new
    // folder_path in details. If we ever want a dedicated
    // "library.move" action, the move handler will need its own SQL
    // path and a new const here.
}

/// One row to write. Builder-shape is overkill for 6 fields; the
/// caller fills the struct inline. `details` is an arbitrary JSON
/// value the caller composes; see `record()` for the serialise
/// strategy.
///
/// `actor_id` is optional because the schema's FK (`REFERENCES users(id)
/// ON DELETE SET NULL`) permits NULL. API handlers always have an
/// authenticated user and pass `Some(&auth.0.sub)`; the CLI has no
/// session and passes `None` so the audit row records the action
/// without faking a user reference (`actor_email` carries `"<cli>"`
/// in that case so the dashboard view stays legible).
pub struct AuditEvent<'a> {
    pub actor_id: Option<&'a str>,
    pub actor_email: &'a str,
    pub action: &'a str,
    pub target_kind: Option<&'a str>,
    pub target_id: Option<&'a str>,
    pub details: Option<JsonValue>,
}

/// Insert one audit row. Best-effort: a database error here is
/// logged at warn level and otherwise dropped. The calling code's
/// own success path is what the user sees; an audit-write failure
/// shouldn't bubble back as a 500.
pub async fn record(pool: &SqlitePool, event: AuditEvent<'_>) {
    let now = chrono::Utc::now().timestamp();
    let details_str = event
        .details
        .as_ref()
        .map(|d| d.to_string())
        .unwrap_or_default();
    let details_param: Option<&str> = if details_str.is_empty() {
        None
    } else {
        Some(&details_str)
    };
    let res = sqlx::query(
        "INSERT INTO audit_log \
           (at, actor_id, actor_email, action, target_kind, target_id, details) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(now)
    .bind(event.actor_id)
    .bind(event.actor_email)
    .bind(event.action)
    .bind(event.target_kind)
    .bind(event.target_id)
    .bind(details_param)
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(
            actor = event.actor_id.unwrap_or("<none>"),
            action = event.action,
            "audit: insert failed: {e}"
        );
    }
}

/// Fetch the email for an actor id. Used by callers building an
/// `AuditEvent` to populate `actor_email` (the denormalised field
/// that survives `users` deletes). Falls back to "<unknown>" on
/// any error so the audit row still goes in with something legible.
pub async fn lookup_actor_email(pool: &SqlitePool, actor_id: &str) -> String {
    sqlx::query_scalar::<_, String>("SELECT email FROM users WHERE id = ?")
        .bind(actor_id)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|_| "<unknown>".to_string())
}

/// What the dashboard's audit page reads. The `actor_email` column
/// makes the listing legible even when the actor row is gone
/// (`actor_id` would be NULL after a user.delete that cascaded).
#[derive(Debug, sqlx::FromRow)]
pub struct AuditRow {
    pub id: i64,
    pub at: i64,
    /// NULL for CLI actions and for actors whose user row was
    /// deleted (the FK is ON DELETE SET NULL). When present, the
    /// dashboard links the actor email to the user's detail page.
    pub actor_id: Option<String>,
    pub actor_email: String,
    pub action: String,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    pub details: Option<String>,
}

/// Actions we hide from the dashboard view (and exclude from its
/// count). folder.reorder used to be recorded; it carries no
/// destructive effect and reads as inscrutable JSON in the audit
/// table, so we filter it everywhere it would surface. Old rows
/// stay in the table for forensic completeness.
pub const HIDDEN_ACTIONS: &[&str] = &["library.folder.reorder"];

/// Comma-quoted list for embedding in a SQL `IN (...)` clause. The
/// values are compile-time constants here, but the helper keeps the
/// quoting honest if the list ever grows.
fn hidden_actions_sql_list() -> String {
    HIDDEN_ACTIONS
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// SQL fragment that excludes hidden actions from a query against
/// the `audit_log` table. Returns an empty string when no actions
/// are hidden so the caller can append unconditionally.
///
/// Checks the rendered list rather than the const itself so clippy
/// doesn't flag the dead branch when HIDDEN_ACTIONS is known to be
/// non-empty - and the protection survives a future change that
/// empties the list (which would otherwise produce invalid SQL
/// like `WHERE action NOT IN ()`).
pub fn hidden_actions_filter_sql(prefix: &str) -> String {
    let list = hidden_actions_sql_list();
    if list.is_empty() {
        String::new()
    } else {
        format!(" {prefix} action NOT IN ({list})")
    }
}

/// Most-recent N entries. The dashboard wraps this for its audit
/// page; the offset + limit are bounded by the caller (so a
/// runaway URL like `?limit=999999999` can't OOM the page).
pub async fn list_recent(pool: &SqlitePool, limit: i64, offset: i64) -> Vec<AuditRow> {
    let sql = format!(
        "SELECT id, at, actor_id, actor_email, action, target_kind, target_id, details \
         FROM audit_log{filter} \
         ORDER BY at DESC, id DESC \
         LIMIT ?1 OFFSET ?2",
        filter = hidden_actions_filter_sql("WHERE"),
    );
    sqlx::query_as::<_, AuditRow>(&sql)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
}
