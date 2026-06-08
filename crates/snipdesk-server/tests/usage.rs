//! Integration tests for the paste-telemetry endpoint.
//!
//! Two correctness invariants to nail down:
//!   1. Deltas accumulate idempotent-per-call. Two POSTs of the same
//!      payload result in 2x the counters - dedupe is the client's
//!      job, not the server's.
//!   2. Cross-user isolation: user B can't bump a personal_snippets
//!      row owned by user A (the owner_id gate makes the UPDATE
//!      match zero rows, the server still returns 204). Library is
//!      shared by design so cross-user UPSERT is fine.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use snipdesk_server::config::MasterKey;
use snipdesk_server::db;
use snipdesk_server::http::{router, AppState};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::Row;
use tower::ServiceExt;

async fn make_state() -> (axum::Router, sqlx::SqlitePool) {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    db::run_migrations(&pool).await.expect("migrations");
    let state = AppState {
        pool: pool.clone(),
        master_key: Arc::new(MasterKey::generate()),
        jwt_secret: "test-jwt-secret".into(),
        oidc_google: None,
        oidc_allowed_schemes: vec!["snipdesk".to_string()],
        secure_cookies: false,
        stats: snipdesk_server::config::StatsConfig::default(),
        fx_cache: std::sync::Arc::new(snipdesk_server::fx::FxCache::default()),
        cors_allowed_origins: Vec::new(),
        brand_name: "SnipDesk".to_string(),
        update_cache: std::sync::Arc::new(snipdesk_server::updater::UpdateCache::default()),
    };
    (router(state), pool)
}

async fn signup(app: &axum::Router, email: &str) -> (String, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/signup")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "email": email,
                        "password": "correcthorsebatterystaple",
                        "display_name": "Tester",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let token = json["token"].as_str().unwrap().to_string();
    let id = json["user"]["id"].as_str().unwrap().to_string();
    (token, id)
}

async fn post_json(
    app: &axum::Router,
    path: &str,
    token: &str,
    body: serde_json::Value,
) -> StatusCode {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    resp.status()
}

#[tokio::test]
async fn report_accumulates_user_totals() {
    let (app, pool) = make_state().await;
    let (token, user_id) = signup(&app, "alice@example.com").await;

    // First flush.
    let s = post_json(
        &app,
        "/api/usage/report",
        &token,
        serde_json::json!({
            "chars_pasted_delta": 1200,
            "snippets_pasted_delta": 5,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // Second flush adds.
    let s = post_json(
        &app,
        "/api/usage/report",
        &token,
        serde_json::json!({
            "chars_pasted_delta": 800,
            "snippets_pasted_delta": 3,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let row = sqlx::query("SELECT chars_pasted, snippets_pasted FROM users WHERE id = ?1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let chars: i64 = row.get("chars_pasted");
    let snips: i64 = row.get("snippets_pasted");
    assert_eq!(chars, 2000);
    assert_eq!(snips, 8);
}

#[tokio::test]
async fn report_rejects_obviously_bogus_deltas() {
    let (app, _pool) = make_state().await;
    let (token, _) = signup(&app, "alice@example.com").await;

    let s = post_json(
        &app,
        "/api/usage/report",
        &token,
        serde_json::json!({"chars_pasted_delta": -5}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let s = post_json(
        &app,
        "/api/usage/report",
        &token,
        serde_json::json!({"chars_pasted_delta": 100_000_000_000_i64}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn report_personal_delta_is_owner_scoped() {
    let (app, pool) = make_state().await;
    let (token_a, _id_a) = signup(&app, "alice@example.com").await;
    let (token_b, _id_b) = signup(&app, "bob@example.com").await;

    // Alice creates a snippet.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/snippets")
                .header("authorization", format!("Bearer {token_a}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "id": "snip-1",
                        "title": "T",
                        "body": "hello world",
                        "tags": [],
                        "folder_path": null,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Bob tries to bump Alice's snippet usage.
    let s = post_json(
        &app,
        "/api/usage/report",
        &token_b,
        serde_json::json!({
            "personal": [{"id": "snip-1", "delta": 99, "last_used": 1717000000}],
        }),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // Snippet usage_count should still be 0 - Bob's bump was silently
    // dropped by the owner_id gate.
    let row = sqlx::query("SELECT usage_count FROM personal_snippets WHERE id = 'snip-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let usage: i64 = row.get("usage_count");
    assert_eq!(usage, 0);

    // Alice's own bump goes through.
    let s = post_json(
        &app,
        "/api/usage/report",
        &token_a,
        serde_json::json!({
            "personal": [{"id": "snip-1", "delta": 3, "last_used": 1717000000}],
        }),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let row = sqlx::query("SELECT usage_count FROM personal_snippets WHERE id = 'snip-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let usage: i64 = row.get("usage_count");
    assert_eq!(usage, 3);
}

#[tokio::test]
async fn report_library_usage_is_per_user_aggregated() {
    let (app, pool) = make_state().await;
    let (token_a, id_a) = signup(&app, "alice@example.com").await;
    let (token_b, id_b) = signup(&app, "bob@example.com").await;

    // Both users hit the same library snippet id; we don't need the
    // library_snippets row to exist for the counter (FK was
    // deliberately omitted in the migration).
    for (token, n) in [(&token_a, 4), (&token_b, 7)] {
        let s = post_json(
            &app,
            "/api/usage/report",
            token,
            serde_json::json!({
                "library": [{"id": "lib-1", "delta": n, "last_used": 1717000000}],
            }),
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
    }

    // Per-user rows present, aggregated.
    let row = sqlx::query(
        "SELECT user_id, usage_count FROM library_usage WHERE snippet_id = 'lib-1' \
         ORDER BY user_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(row.len(), 2);
    let mut found_a = false;
    let mut found_b = false;
    for r in row {
        let uid: String = r.get("user_id");
        let n: i64 = r.get("usage_count");
        if uid == id_a {
            assert_eq!(n, 4);
            found_a = true;
        } else if uid == id_b {
            assert_eq!(n, 7);
            found_b = true;
        }
    }
    assert!(found_a && found_b);

    // Total across team.
    let total: i64 =
        sqlx::query_scalar("SELECT SUM(usage_count) FROM library_usage WHERE snippet_id = 'lib-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(total, 11);
}

#[tokio::test]
async fn report_requires_auth() {
    let (app, _pool) = make_state().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/usage/report")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"chars_pasted_delta": 1}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
