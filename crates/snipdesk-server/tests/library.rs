//! Integration tests for the shared library endpoints.
//!
//! Library behaviour to lock down here:
//!   - Any signed-in user can GET; writes require role=admin.
//!   - The version counter is global (not per-user), so two members
//!     pulling with the same `since` see the same stream.
//!   - Tombstones surface to all members.
//!   - Optimistic concurrency works the same way as personal_snippets.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use snipdesk_server::config::MasterKey;
use snipdesk_server::db;
use snipdesk_server::http::{router, AppState};
use sqlx::sqlite::SqlitePoolOptions;
use tower::ServiceExt;

async fn make_app() -> axum::Router {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    db::run_migrations(&pool).await.expect("migrations");
    let state = AppState {
        pool,
        master_key: Arc::new(MasterKey::generate()),
        jwt_secret: "test-jwt-secret".into(),
        oidc_google: None,
        secure_cookies: false,
        stats: snipdesk_server::config::StatsConfig::default(),
        fx_cache: std::sync::Arc::new(snipdesk_server::fx::FxCache::default()),
        cors_allowed_origins: Vec::new(),
    };
    router(state)
}

/// First signup auto-promotes to admin; subsequent signups are members.
/// Tests rely on this so they can deterministically grab tokens of each
/// role from the same harness.
async fn signup(app: &axum::Router, email: &str) -> String {
    let body = serde_json::json!({
        "email": email,
        "password": "correcthorsebatterystaple",
        "display_name": "Test",
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/signup")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    json["token"].as_str().unwrap().to_string()
}

async fn request(
    app: &axum::Router,
    method: &str,
    path: &str,
    token: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"));
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

// Admin can create + members can read it back. The body returned via
// sync matches what the admin posted - proves the wire shape.
#[tokio::test]
async fn admin_creates_member_reads() {
    let app = make_app().await;
    let admin = signup(&app, "admin@example.com").await;
    let member = signup(&app, "member@example.com").await;

    let (status, created) = request(
        &app,
        "POST",
        "/api/library",
        &admin,
        Some(serde_json::json!({
            "id": "greet-1",
            "title": "Greeting",
            "body": "Hi {name}!",
            "tags": ["intro"],
            "folder_path": "Replies",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["version"], 1);

    // Member sees it via sync.
    let (status, list) = request(&app, "GET", "/api/library", &member, None).await;
    assert_eq!(status, StatusCode::OK);
    let snip = &list["snippets"][0];
    assert_eq!(snip["id"], "greet-1");
    assert_eq!(snip["payload"]["title"], "Greeting");
    assert_eq!(snip["payload"]["body"], "Hi {name}!");
    assert_eq!(snip["payload"]["tags"][0], "intro");
    assert_eq!(snip["payload"]["folder_path"], "Replies");
    assert_eq!(list["high_water_mark"], 1);
}

// Non-admin writes are rejected with 403, regardless of which write
// verb. This is the central authorization invariant - the read path is
// shared, the write path is admin-only, and we want that to be tested
// directly rather than implied by passing other tests.
#[tokio::test]
async fn members_cannot_write() {
    let app = make_app().await;
    let _admin = signup(&app, "admin@example.com").await;
    let member = signup(&app, "member@example.com").await;

    let payload = serde_json::json!({
        "id": "x",
        "title": "blocked",
        "body": "",
        "tags": [],
        "folder_path": null,
    });

    let (status, body) = request(&app, "POST", "/api/library", &member, Some(payload)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "admin_required");

    let (status, _) = request(
        &app,
        "PUT",
        "/api/library/anything",
        &member,
        Some(serde_json::json!({
            "expected_version": 1,
            "title": "x", "body": "", "tags": [], "folder_path": null,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = request(&app, "DELETE", "/api/library/x", &member, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// `since` cursor filters strictly - the same invariant we lean on for
// personal snippets. Library uses a global counter rather than per-user
// so two members pulling with `since=2` see the same payload.
#[tokio::test]
async fn since_filter_excludes_seen_versions() {
    let app = make_app().await;
    let admin = signup(&app, "admin@example.com").await;
    let member = signup(&app, "member@example.com").await;

    for (id, title) in [("a", "Apple"), ("b", "Banana"), ("c", "Cherry")] {
        let (s, _) = request(
            &app,
            "POST",
            "/api/library",
            &admin,
            Some(serde_json::json!({
                "id": id, "title": title, "body": "", "tags": [], "folder_path": null,
            })),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
    }

    let (_, list) = request(&app, "GET", "/api/library?since=2", &member, None).await;
    let ids: Vec<&str> = list["snippets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["c"]);
    assert_eq!(list["high_water_mark"], 3);
}

// Delete writes a tombstone (visible via the sync stream, payload null)
// and the second delete is idempotent.
#[tokio::test]
async fn delete_emits_tombstone_then_is_idempotent() {
    let app = make_app().await;
    let admin = signup(&app, "admin@example.com").await;
    let member = signup(&app, "member@example.com").await;

    request(
        &app,
        "POST",
        "/api/library",
        &admin,
        Some(serde_json::json!({
            "id": "doomed", "title": "bye", "body": "", "tags": [], "folder_path": null,
        })),
    )
    .await;

    let (status, _) = request(&app, "DELETE", "/api/library/doomed", &admin, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, list) = request(&app, "GET", "/api/library", &member, None).await;
    let snip = &list["snippets"][0];
    assert_eq!(snip["is_deleted"], true);
    assert!(snip["payload"].is_null());

    let (status, _) = request(&app, "DELETE", "/api/library/doomed", &admin, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// Concurrent admin edits: the stale PUT must fail with 409 so the
// client refetches. Mirrors the personal_snippets contract; admin
// tooling can share conflict-handling code.
#[tokio::test]
async fn update_rejects_stale_expected_version() {
    let app = make_app().await;
    let admin = signup(&app, "admin@example.com").await;

    request(
        &app,
        "POST",
        "/api/library",
        &admin,
        Some(serde_json::json!({
            "id": "x", "title": "v1", "body": "", "tags": [], "folder_path": null,
        })),
    )
    .await;

    let (status, updated) = request(
        &app,
        "PUT",
        "/api/library/x",
        &admin,
        Some(serde_json::json!({
            "expected_version": 1,
            "title": "v2", "body": "", "tags": [], "folder_path": null,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["version"], 2);

    let (status, err) = request(
        &app,
        "PUT",
        "/api/library/x",
        &admin,
        Some(serde_json::json!({
            "expected_version": 1,
            "title": "v3", "body": "", "tags": [], "folder_path": null,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err["error"], "version_conflict");
}

// Every library endpoint requires a token. Easy to regress if someone
// copies a route from a future health/metrics endpoint.
#[tokio::test]
async fn library_endpoints_require_auth() {
    let app = make_app().await;
    let no_auth = |method: &'static str, path: &'static str, body: Option<serde_json::Value>| {
        let app = app.clone();
        async move {
            let mut req = Request::builder().method(method).uri(path);
            let body_b = match body {
                Some(v) => {
                    req = req.header("content-type", "application/json");
                    Body::from(v.to_string())
                }
                None => Body::empty(),
            };
            app.oneshot(req.body(body_b).unwrap())
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(
        no_auth("GET", "/api/library", None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        no_auth(
            "POST",
            "/api/library",
            Some(serde_json::json!({"id":"x","title":"","body":"","tags":[],"folder_path":null}))
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        no_auth("DELETE", "/api/library/x", None).await,
        StatusCode::UNAUTHORIZED
    );
}
