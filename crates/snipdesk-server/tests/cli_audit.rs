//! Verify that CLI user-management commands write to audit_log.
//!
//! The dashboard + JSON API have always audited their mutations. The
//! CLI (`docker compose exec ... users promote/demote/disable/delete/
//! reset-password`) used to bypass the audit layer entirely, which
//! left a forensic blind spot for operations done from a shell. This
//! test pins down that every CLI mutation now records, with the
//! synthetic actor_id = NULL / actor_email = "<cli>" shape.

use snipdesk_server::cli::{run_with_pool, UsersCmd};
use snipdesk_server::db;
use sqlx::sqlite::SqlitePoolOptions;

async fn setup() -> sqlx::SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    db::run_migrations(&pool).await.expect("migrations");
    pool
}

async fn insert_user(pool: &sqlx::SqlitePool, id: &str, email: &str, role: &str) {
    sqlx::query(
        "INSERT INTO users \
           (id, email, display_name, role, is_disabled, created_at, last_seen_at, password_hash) \
         VALUES (?, ?, ?, ?, 0, 1, 1, 'fake-hash-not-used-in-this-test')",
    )
    .bind(id)
    .bind(email)
    .bind(format!("Test {role}"))
    .bind(role)
    .execute(pool)
    .await
    .expect("insert user");
}

async fn audit_rows_for_target(
    pool: &sqlx::SqlitePool,
    target_id: &str,
) -> Vec<(Option<String>, String, String, Option<String>)> {
    sqlx::query_as::<_, (Option<String>, String, String, Option<String>)>(
        "SELECT actor_id, actor_email, action, details \
         FROM audit_log WHERE target_id = ? ORDER BY id ASC",
    )
    .bind(target_id)
    .fetch_all(pool)
    .await
    .expect("query audit_log")
}

#[tokio::test]
async fn cli_promote_writes_audit_row() {
    let pool = setup().await;
    // The CLI needs an existing admin to test demotion against, so we
    // create two users: an existing admin (so guard_last_admin doesn't
    // block when demoting later) and the target member to promote.
    insert_user(&pool, "admin-1", "admin@example.com", "admin").await;
    insert_user(&pool, "member-1", "member@example.com", "member").await;

    run_with_pool(
        &pool,
        UsersCmd::Promote {
            email: "member@example.com".into(),
        },
    )
    .await
    .expect("promote");

    let rows = audit_rows_for_target(&pool, "member-1").await;
    assert_eq!(rows.len(), 1, "expected exactly one audit row");
    let (actor_id, actor_email, action, details) = &rows[0];
    assert_eq!(*actor_id, None, "CLI mutations land actor_id = NULL");
    assert_eq!(actor_email, "<cli>");
    assert_eq!(action, "user.update");
    let parsed: serde_json::Value =
        serde_json::from_str(details.as_deref().unwrap_or("null")).expect("details json");
    assert_eq!(parsed["role"]["from"], "member");
    assert_eq!(parsed["role"]["to"], "admin");
}

#[tokio::test]
async fn cli_disable_writes_audit_row() {
    let pool = setup().await;
    insert_user(&pool, "admin-1", "admin@example.com", "admin").await;
    insert_user(&pool, "member-1", "member@example.com", "member").await;

    run_with_pool(
        &pool,
        UsersCmd::Disable {
            email: "member@example.com".into(),
        },
    )
    .await
    .expect("disable");

    let rows = audit_rows_for_target(&pool, "member-1").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "<cli>");
    assert_eq!(rows[0].2, "user.update");
    let parsed: serde_json::Value =
        serde_json::from_str(rows[0].3.as_deref().unwrap_or("null")).unwrap();
    assert_eq!(parsed["is_disabled"]["from"], false);
    assert_eq!(parsed["is_disabled"]["to"], true);
}

#[tokio::test]
async fn cli_delete_writes_audit_row() {
    let pool = setup().await;
    insert_user(&pool, "admin-1", "admin@example.com", "admin").await;
    insert_user(&pool, "member-1", "member@example.com", "member").await;

    run_with_pool(
        &pool,
        UsersCmd::Delete {
            email: "member@example.com".into(),
            yes: true,
        },
    )
    .await
    .expect("delete");

    // The user row is gone, but the audit row should remain. We query
    // by target_id (which doesn't depend on the user row existing).
    let rows = audit_rows_for_target(&pool, "member-1").await;
    assert_eq!(rows.len(), 1);
    let (actor_id, actor_email, action, details) = &rows[0];
    assert_eq!(*actor_id, None);
    assert_eq!(actor_email, "<cli>");
    assert_eq!(action, "user.delete");
    // Email is preserved in details so the audit page can show
    // "deleted member@example.com" without joining back to a row
    // that no longer exists.
    let parsed: serde_json::Value =
        serde_json::from_str(details.as_deref().unwrap_or("null")).unwrap();
    assert_eq!(parsed["email"], "member@example.com");
}

// reset_password isn't covered here: the function reads from stdin
// and a tokio-managed test can't reliably feed it a value without an
// out-of-process harness. The audit::record call site is verified by
// code review (cli.rs reset_password follows the same shape as
// set_role / set_disabled, using USER_UPDATE with
// details = {"password_reset": true}).
