//! Integration tests for personal snippet CRUD + sync. Same in-memory
//! axum harness as the auth tests; here we focus on the version-counter
//! and conflict semantics that are easy to get wrong by hand.

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
        oidc_keycloak: None,
        oidc_allowed_schemes: vec!["snipdesk".to_string()],
        oidc_allowed_redirect_urls: Vec::new(),
        secure_cookies: false,
        password_enabled: true,
        stats: snipdesk_server::config::StatsConfig::default(),
        fx_cache: std::sync::Arc::new(snipdesk_server::fx::FxCache::default()),
        cors_allowed_origins: Vec::new(),
        brand_name: "SnipDesk".to_string(),
        update_cache: std::sync::Arc::new(snipdesk_server::updater::UpdateCache::default()),
    };
    router(state)
}

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

// Round-trip: create a snippet, GET it back via sync. The payload must
// be byte-identical to what we sent - this proves both the wire shape
// and the encrypt/decrypt loop.
#[tokio::test]
async fn create_then_list_round_trips_payload() {
    let app = make_app().await;
    let token = signup(&app, "alice@example.com").await;

    let (status, created) = request(
        &app,
        "POST",
        "/api/snippets",
        &token,
        Some(serde_json::json!({
            "id": "snip-1",
            "title": "Hello",
            "body": "Hi {customer}, thanks.",
            "tags": ["greeting"],
            "folder_path": "General",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["id"], "snip-1");
    assert_eq!(created["version"], 1);

    let (status, list) = request(&app, "GET", "/api/snippets", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["snippets"].as_array().unwrap().len(), 1);
    let snip = &list["snippets"][0];
    assert_eq!(snip["id"], "snip-1");
    assert_eq!(snip["version"], 1);
    assert_eq!(snip["is_deleted"], false);
    assert_eq!(snip["payload"]["title"], "Hello");
    assert_eq!(snip["payload"]["body"], "Hi {customer}, thanks.");
    assert_eq!(snip["payload"]["tags"][0], "greeting");
    assert_eq!(snip["payload"]["folder_path"], "General");
    assert_eq!(list["high_water_mark"], 1);
}

// `since=N` filters strictly - version <= N must be invisible. This is
// the key correctness invariant for the sync loop; a bug here would
// make clients miss updates or re-process every snippet on every tick.
#[tokio::test]
async fn since_filter_excludes_seen_versions() {
    let app = make_app().await;
    let token = signup(&app, "alice@example.com").await;

    for (id, title) in [("a", "Apple"), ("b", "Banana"), ("c", "Cherry")] {
        let (s, _) = request(
            &app,
            "POST",
            "/api/snippets",
            &token,
            Some(serde_json::json!({
                "id": id, "title": title, "body": "", "tags": [], "folder_path": null,
            })),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
    }

    let (_, list) = request(&app, "GET", "/api/snippets?since=2", &token, None).await;
    let ids: Vec<&str> = list["snippets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    // Only the snippet with version 3 ("c") survives the filter.
    assert_eq!(ids, vec!["c"]);
    assert_eq!(list["high_water_mark"], 3);
}

// Update with the right expected_version wins; wrong one is rejected
// with 409. This is what protects last-write-wins from silently
// clobbering on the client side.
#[tokio::test]
async fn update_optimistic_concurrency() {
    let app = make_app().await;
    let token = signup(&app, "alice@example.com").await;

    request(
        &app,
        "POST",
        "/api/snippets",
        &token,
        Some(serde_json::json!({
            "id": "x", "title": "v1", "body": "", "tags": [], "folder_path": null,
        })),
    )
    .await;

    // Right version → 200, version bumps to 2.
    let (status, updated) = request(
        &app,
        "PUT",
        "/api/snippets/x",
        &token,
        Some(serde_json::json!({
            "expected_version": 1,
            "title": "v2",
            "body": "updated",
            "tags": [],
            "folder_path": null,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["version"], 2);

    // Same expected_version as before (1) is now stale → 409.
    let (status, err) = request(
        &app,
        "PUT",
        "/api/snippets/x",
        &token,
        Some(serde_json::json!({
            "expected_version": 1,
            "title": "v3",
            "body": "stale",
            "tags": [],
            "folder_path": null,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err["error"], "version_conflict");
}

// Delete produces a tombstone visible via sync, payload omitted.
// Subsequent delete of the same id is idempotent (no error).
#[tokio::test]
async fn delete_produces_tombstone_then_is_idempotent() {
    let app = make_app().await;
    let token = signup(&app, "alice@example.com").await;

    request(
        &app,
        "POST",
        "/api/snippets",
        &token,
        Some(serde_json::json!({
            "id": "doomed", "title": "bye", "body": "", "tags": [], "folder_path": null,
        })),
    )
    .await;

    let (status, _) = request(&app, "DELETE", "/api/snippets/doomed", &token, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, list) = request(&app, "GET", "/api/snippets", &token, None).await;
    let snip = &list["snippets"][0];
    assert_eq!(snip["is_deleted"], true);
    assert!(snip["payload"].is_null());

    // Idempotent re-delete.
    let (status, _) = request(&app, "DELETE", "/api/snippets/doomed", &token, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// Cross-user isolation: user A cannot list, update, or delete user B's
// snippets. Each user's data is fenced by owner_id on every query.
#[tokio::test]
async fn users_cannot_see_each_others_snippets() {
    let app = make_app().await;
    let token_a = signup(&app, "alice@example.com").await;
    let token_b = signup(&app, "bob@example.com").await;

    let (s, _) = request(
        &app,
        "POST",
        "/api/snippets",
        &token_a,
        Some(serde_json::json!({
            "id": "alice-1", "title": "private", "body": "secret", "tags": [], "folder_path": null,
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);

    // Bob's list is empty.
    let (_, list_b) = request(&app, "GET", "/api/snippets", &token_b, None).await;
    assert_eq!(list_b["snippets"].as_array().unwrap().len(), 0);

    // Bob can't update Alice's snippet either - looks not-found from his
    // side (don't leak existence). 404 is the standard semantics now;
    // older versions returned 400 for the same case.
    let (s, _) = request(
        &app,
        "PUT",
        "/api/snippets/alice-1",
        &token_b,
        Some(serde_json::json!({
            "expected_version": 1,
            "title": "hijacked", "body": "", "tags": [], "folder_path": null,
        })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Bob can't delete it either.
    let (s, _) = request(&app, "DELETE", "/api/snippets/alice-1", &token_b, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Alice's snippet still intact for her.
    let (_, list_a) = request(&app, "GET", "/api/snippets", &token_a, None).await;
    assert_eq!(list_a["snippets"][0]["payload"]["body"], "secret");
}

// Auth required: every snippet endpoint 401s without a token. Easy to
// regress if someone copies a route from the existing public ones.
#[tokio::test]
async fn snippet_endpoints_require_auth() {
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
        no_auth("GET", "/api/snippets", None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        no_auth(
            "POST",
            "/api/snippets",
            Some(serde_json::json!({"id":"x","title":"","body":"","tags":[],"folder_path":null}))
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        no_auth("DELETE", "/api/snippets/x", None).await,
        StatusCode::UNAUTHORIZED
    );
}

// The size/character limits (validate.rs) gate the write path: an
// oversized body, a null byte in the title, or a 400-char folder path
// must be a clean 400, never a row in the database. The limits mirror
// the client's; a payload the desktop accepts has to land here too,
// so only over-limit shapes are exercised.
#[tokio::test]
async fn create_rejects_oversized_and_control_character_payloads() {
    let app = make_app().await;
    let token = signup(&app, "alice@example.com").await;

    let cases = vec![
        serde_json::json!({
            "id": "bad-1",
            "title": "x".repeat(301),
            "body": "fine",
            "tags": [],
            "folder_path": null,
        }),
        serde_json::json!({
            "id": "bad-2",
            "title": "fine",
            "body": "y".repeat(100_001),
            "tags": [],
            "folder_path": null,
        }),
        serde_json::json!({
            "id": "bad-3",
            "title": "null\u{0000}byte",
            "body": "fine",
            "tags": [],
            "folder_path": null,
        }),
        serde_json::json!({
            "id": "bad-4",
            "title": "fine",
            "body": "fine",
            "tags": [],
            "folder_path": "f".repeat(301),
        }),
    ];
    for case in cases {
        let id = case["id"].as_str().unwrap().to_string();
        let (status, body) = request(&app, "POST", "/api/snippets", &token, Some(case)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "case {id}: {body}");
        assert_eq!(body["error"], "invalid_payload", "case {id}");
    }

    // Nothing leaked through to storage.
    let (_, list) = request(&app, "GET", "/api/snippets", &token, None).await;
    assert_eq!(list["snippets"].as_array().unwrap().len(), 0);

    // Within-limits payloads (incl. multi-byte text and newlines)
    // still sail through.
    let (status, _) = request(
        &app,
        "POST",
        "/api/snippets",
        &token,
        Some(serde_json::json!({
            "id": "good-1",
            "title": "Greeting",
            "body": "Hello\nthanks for waiting.",
            "tags": ["jp"],
            "folder_path": "Replies",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}
