//! Integration tests for the auth flow. Each test builds a fresh
//! in-memory SQLite-backed router and exercises endpoints via
//! `tower::ServiceExt::oneshot` - no TCP listener, no real network.
//!
//! What this catches that unit tests don't:
//!   - Wiring (routes pointed at the right handlers).
//!   - JSON serialization shape (the wire contract).
//!   - SQL queries actually run (e.g. column-name typos).
//!   - The auth extractor's 401 paths.

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
    // In-memory SQLite: a fresh DB per test, never touches disk.
    let pool = SqlitePoolOptions::new()
        .max_connections(1) // serialize writes; in-memory SQLite doesn't share across conns
        .connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    db::run_migrations(&pool).await.expect("migrations");
    let state = AppState {
        pool,
        master_key: Arc::new(MasterKey::generate()),
        jwt_secret: "test-jwt-secret-not-for-production".into(),
    };
    router(state)
}

async fn post_json(
    app: &axum::Router,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("build request"),
        )
        .await
        .expect("oneshot");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

async fn get_with_bearer(
    app: &axum::Router,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder().method("GET").uri(path);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).expect("build request"))
        .await
        .expect("oneshot");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

// First signup gets role=admin so the operator can manage the server
// without a separate bootstrap step. Subsequent signups are 'member'.
#[tokio::test]
async fn first_signup_is_admin_then_subsequent_are_members() {
    let app = make_app().await;
    let (status, body) = post_json(
        &app,
        "/api/auth/signup",
        serde_json::json!({
            "email": "first@example.com",
            "password": "averylongpassword",
            "display_name": "First User",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["user"]["role"], "admin");
    assert!(body["token"].as_str().is_some());

    let (status, body) = post_json(
        &app,
        "/api/auth/signup",
        serde_json::json!({
            "email": "second@example.com",
            "password": "anotherlongpassword",
            "display_name": "Second User",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["user"]["role"], "member");
}

// Validation surface: weak password, missing display name, bad email
// must all 400 before we touch the DB.
#[tokio::test]
async fn signup_validation_rejects_weak_or_malformed_input() {
    let app = make_app().await;
    let (status, body) = post_json(
        &app,
        "/api/auth/signup",
        serde_json::json!({"email":"u@e.com","password":"short","display_name":"x"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "weak_password");

    let (status, body) = post_json(
        &app,
        "/api/auth/signup",
        serde_json::json!({"email":"not-an-email","password":"averylongpassword","display_name":"x"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_email");
}

// Logging in twice with the same correct credentials should both
// succeed and return distinct tokens (timestamps differ).
#[tokio::test]
async fn login_returns_valid_token_for_correct_credentials() {
    let app = make_app().await;
    post_json(
        &app,
        "/api/auth/signup",
        serde_json::json!({
            "email": "alice@example.com",
            "password": "correcthorsebatterystaple",
            "display_name": "Alice",
        }),
    )
    .await;
    let (status, body) = post_json(
        &app,
        "/api/auth/login",
        serde_json::json!({
            "email": "alice@example.com",
            "password": "correcthorsebatterystaple",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = body["token"].as_str().expect("token in response");
    assert!(!token.is_empty());
}

// Bad password AND unknown email should both return the same error
// code/message. Asserting on the literal so a refactor that
// accidentally adds "user not found" vs "wrong password" trips this.
#[tokio::test]
async fn login_failures_use_one_generic_message() {
    let app = make_app().await;
    post_json(
        &app,
        "/api/auth/signup",
        serde_json::json!({
            "email": "bob@example.com",
            "password": "correcthorsebatterystaple",
            "display_name": "Bob",
        }),
    )
    .await;

    let (status1, body1) = post_json(
        &app,
        "/api/auth/login",
        serde_json::json!({"email":"bob@example.com","password":"wrong-password-here"}),
    )
    .await;
    let (status2, body2) = post_json(
        &app,
        "/api/auth/login",
        serde_json::json!({"email":"nobody@example.com","password":"wrong-password-here"}),
    )
    .await;

    assert_eq!(status1, StatusCode::UNAUTHORIZED);
    assert_eq!(status2, StatusCode::UNAUTHORIZED);
    assert_eq!(body1["error"], "invalid_credentials");
    assert_eq!(body2["error"], "invalid_credentials");
    assert_eq!(body1["message"], body2["message"]);
}

// /api/me with a valid token returns the authenticated user; without a
// token returns 401 (the AuthUser extractor short-circuits).
#[tokio::test]
async fn me_requires_valid_token() {
    let app = make_app().await;
    let (_, signup) = post_json(
        &app,
        "/api/auth/signup",
        serde_json::json!({
            "email": "carol@example.com",
            "password": "correcthorsebatterystaple",
            "display_name": "Carol",
        }),
    )
    .await;
    let token = signup["token"].as_str().unwrap();

    let (status, body) = get_with_bearer(&app, "/api/me", Some(token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user"]["email"], "carol@example.com");

    let (status, _) = get_with_bearer(&app, "/api/me", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = get_with_bearer(&app, "/api/me", Some("not.a.real.token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "invalid_token");
}

// Duplicate email signup must conflict (409) instead of silently
// overwriting or 500'ing.
#[tokio::test]
async fn signup_rejects_duplicate_email() {
    let app = make_app().await;
    let body = serde_json::json!({
        "email": "dup@example.com",
        "password": "correcthorsebatterystaple",
        "display_name": "Dup",
    });
    let (s1, _) = post_json(&app, "/api/auth/signup", body.clone()).await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, b2) = post_json(&app, "/api/auth/signup", body).await;
    assert_eq!(s2, StatusCode::CONFLICT);
    assert_eq!(b2["error"], "email_taken");
}
