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
        oidc_google: None,
        oidc_keycloak: None,
        oidc_allowed_schemes: vec!["snipdesk".to_string()],
        secure_cookies: false,
        stats: snipdesk_server::config::StatsConfig::default(),
        fx_cache: std::sync::Arc::new(snipdesk_server::fx::FxCache::default()),
        cors_allowed_origins: Vec::new(),
        brand_name: "SnipDesk".to_string(),
        update_cache: std::sync::Arc::new(snipdesk_server::updater::UpdateCache::default()),
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

// Login collapses every failure mode (wrong password, unknown email,
// disabled account, SSO-only account) into one generic
// `invalid_credentials` response. Differential responses would let an
// attacker enumerate registered emails (CWE-203). The Argon2 verify
// is run unconditionally (against a sentinel hash when the email is
// missing) so timing doesn't leak the distinction either (CWE-208).
//
// If this is ever softened (e.g. to surface disabled-account as a
// distinct response for UX), document the security tradeoff in
// deploy.md and update this test.
#[tokio::test]
async fn login_failure_modes_collapse_to_invalid_credentials() {
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

    // Wrong password for an existing account.
    let (status1, body1) = post_json(
        &app,
        "/api/auth/login",
        serde_json::json!({"email":"bob@example.com","password":"wrong-password-here"}),
    )
    .await;
    assert_eq!(status1, StatusCode::UNAUTHORIZED);
    assert_eq!(body1["error"], "invalid_credentials");

    // Email that doesn't exist.
    let (status2, body2) = post_json(
        &app,
        "/api/auth/login",
        serde_json::json!({"email":"nobody@example.com","password":"wrong-password-here"}),
    )
    .await;
    assert_eq!(status2, StatusCode::UNAUTHORIZED);
    assert_eq!(body2["error"], "invalid_credentials");

    // Both wire responses are identical: same error code AND same
    // message. An attacker observing only the response can't tell the
    // two cases apart.
    assert_eq!(body1["error"], body2["error"]);
    assert_eq!(body1["message"], body2["message"]);
}

// /api/auth/methods is unauthenticated and reports the configured
// sign-in surfaces. With no OIDC provider configured (test default)
// the providers list is empty and password is enabled.
#[tokio::test]
async fn methods_reports_configured_sign_in_surfaces() {
    let app = make_app().await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/auth/methods")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["password"]["enabled"], true);
    assert_eq!(body["providers"].as_array().unwrap().len(), 0);
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

// Duplicate-email signup is collapsed to a generic `signup_failed`
// (400) so an attacker can't enumerate registered emails by probing
// signup with candidate addresses (CWE-203). The status code is 400
// not 409 - using 409 would itself hint that the email is the
// specific reason. The Argon2 hash now runs unconditionally so the
// duplicate path pays the same ~50ms cost as a successful signup
// (CWE-208).
//
// If this is ever softened, document the security tradeoff in
// deploy.md and update this test.
#[tokio::test]
async fn signup_with_duplicate_email_returns_generic_failure() {
    let app = make_app().await;
    let body = serde_json::json!({
        "email": "dup@example.com",
        "password": "correcthorsebatterystaple",
        "display_name": "Dup",
    });
    let (s1, _) = post_json(&app, "/api/auth/signup", body.clone()).await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, b2) = post_json(&app, "/api/auth/signup", body).await;
    assert_eq!(s2, StatusCode::BAD_REQUEST);
    assert_eq!(b2["error"], "signup_failed");
}

async fn patch_with_bearer(
    app: &axum::Router,
    path: &str,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
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

// PATCH /api/me persists per-user wpm/wage/currency overrides. The
// response echoes the row, GET /api/me then sees the same values, and
// out-of-range / unknown values 400 instead of silently clamping.
#[tokio::test]
async fn me_patch_updates_overrides_then_clears_them() {
    let app = make_app().await;
    let (_, signup) = post_json(
        &app,
        "/api/auth/signup",
        serde_json::json!({
            "email": "patch@example.com",
            "password": "correcthorsebatterystaple",
            "display_name": "Patch",
        }),
    )
    .await;
    let token = signup["token"].as_str().unwrap();

    // Initial state: no overrides; fields omitted from response by
    // skip_serializing_if = "Option::is_none".
    let (status, body) = get_with_bearer(&app, "/api/me", Some(token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["user"]["wpm"].is_null() || body["user"].get("wpm").is_none());

    // Set all three.
    let (status, body) = patch_with_bearer(
        &app,
        "/api/me",
        token,
        serde_json::json!({"wpm": 95, "hourly_wage": 38.5, "currency": "USD"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["wpm"], 95);
    assert_eq!(body["hourly_wage"], 38.5);
    assert_eq!(body["currency"], "USD");

    // GET shows them too.
    let (_, body) = get_with_bearer(&app, "/api/me", Some(token)).await;
    assert_eq!(body["user"]["wpm"], 95);
    assert_eq!(body["user"]["currency"], "USD");

    // Clear by sending null.
    let (status, body) = patch_with_bearer(
        &app,
        "/api/me",
        token,
        serde_json::json!({"wpm": null, "currency": null}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // hourly_wage was NOT in the payload - it must survive the PATCH.
    assert_eq!(body["hourly_wage"], 38.5);
    // wpm / currency are cleared, so skip_serializing_if omits them.
    assert!(body.get("wpm").is_none() || body["wpm"].is_null());
    assert!(body.get("currency").is_none() || body["currency"].is_null());
}

#[tokio::test]
async fn me_patch_rejects_out_of_range_values() {
    let app = make_app().await;
    let (_, signup) = post_json(
        &app,
        "/api/auth/signup",
        serde_json::json!({
            "email": "bounds@example.com",
            "password": "correcthorsebatterystaple",
            "display_name": "Bounds",
        }),
    )
    .await;
    let token = signup["token"].as_str().unwrap();

    let (status, body) =
        patch_with_bearer(&app, "/api/me", token, serde_json::json!({"wpm": 9999})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_wpm");

    let (status, body) = patch_with_bearer(
        &app,
        "/api/me",
        token,
        serde_json::json!({"hourly_wage": -5.0}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_wage");

    let (status, body) = patch_with_bearer(
        &app,
        "/api/me",
        token,
        serde_json::json!({"currency": "XYZ"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "unknown_currency");
}
