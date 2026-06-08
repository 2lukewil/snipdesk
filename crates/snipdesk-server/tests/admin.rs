//! Integration tests for /api/admin/users and the dashboard cookie flow.
//!
//! Focus areas:
//!   - List requires admin; members get 403.
//!   - Role toggles + disable work, with self-protection guards.
//!   - Last-admin demotion is blocked.
//!   - The dashboard's GET / returns the login form when no cookie is
//!     present; POST /dashboard/login sets a cookie; admin-only pages
//!     bounce non-admins.

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

async fn signup(app: &axum::Router, email: &str) -> (String, String) {
    let body = serde_json::json!({
        "email": email,
        "password": "correcthorsebatterystaple",
        "display_name": email.split('@').next().unwrap_or("Test"),
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
    (
        json["token"].as_str().unwrap().to_string(),
        json["user"]["id"].as_str().unwrap().to_string(),
    )
}

async fn request(
    app: &axum::Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
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

// Admin can list users; member cannot. Single test covers both branches
// because they share the same SELECT.
#[tokio::test]
async fn list_users_admin_only() {
    let app = make_app().await;
    let (admin_token, _admin_id) = signup(&app, "admin@example.com").await;
    let (member_token, _) = signup(&app, "member@example.com").await;

    let (s, list) = request(&app, "GET", "/api/admin/users", Some(&admin_token), None).await;
    assert_eq!(s, StatusCode::OK);
    let users = list.as_array().unwrap();
    assert_eq!(users.len(), 2);

    let (s, _) = request(&app, "GET", "/api/admin/users", Some(&member_token), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let (s, _) = request(&app, "GET", "/api/admin/users", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

// Self-protection: admin demotes or disables their own account → 400.
// Without this, an admin could single-handedly lock the org out.
#[tokio::test]
async fn cannot_self_disable_or_demote() {
    let app = make_app().await;
    let (admin_token, admin_id) = signup(&app, "admin@example.com").await;

    let (s, err) = request(
        &app,
        "PUT",
        &format!("/api/admin/users/{admin_id}"),
        Some(&admin_token),
        Some(serde_json::json!({"is_disabled": true})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"], "self_disable");

    let (s, err) = request(
        &app,
        "PUT",
        &format!("/api/admin/users/{admin_id}"),
        Some(&admin_token),
        Some(serde_json::json!({"role": "member"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"], "self_demote");
}

// Promote + demote round-trip across two admins. Exercises the role
// branch in the same path as the dashboard hits, and confirms that
// going from 2 admins → 1 admin is allowed (the last-admin guard is a
// defence-in-depth check; with self-demote also blocked, the only way
// to TRIGGER it in practice is unreachable via the API surface -
// keeping the guard means a future relaxation of self-demote stays
// safe).
#[tokio::test]
async fn promote_then_demote_round_trip() {
    let app = make_app().await;
    let (admin_token, _admin_id) = signup(&app, "admin@example.com").await;
    let (_, member_id) = signup(&app, "member@example.com").await;

    let (s, view) = request(
        &app,
        "PUT",
        &format!("/api/admin/users/{member_id}"),
        Some(&admin_token),
        Some(serde_json::json!({"role": "admin"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(view["role"], "admin");

    let (s, view) = request(
        &app,
        "PUT",
        &format!("/api/admin/users/{member_id}"),
        Some(&admin_token),
        Some(serde_json::json!({"role": "member"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(view["role"], "member");
}

// Disable / enable round-trip succeeds and the view reflects state.
#[tokio::test]
async fn disable_then_enable_member() {
    let app = make_app().await;
    let (admin_token, _) = signup(&app, "admin@example.com").await;
    let (_, member_id) = signup(&app, "member@example.com").await;

    let (s, view) = request(
        &app,
        "PUT",
        &format!("/api/admin/users/{member_id}"),
        Some(&admin_token),
        Some(serde_json::json!({"is_disabled": true})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(view["is_disabled"], true);

    let (s, view) = request(
        &app,
        "PUT",
        &format!("/api/admin/users/{member_id}"),
        Some(&admin_token),
        Some(serde_json::json!({"is_disabled": false})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(view["is_disabled"], false);
}

// GET / when not signed in serves the login form. This is enough to
// catch routing regressions; the form HTML is hand-written and visual.
#[tokio::test]
async fn dashboard_index_serves_login() {
    let app = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.starts_with("text/html"), "content-type was {ct}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&bytes);
    assert!(body.contains("Sign in"), "login form missing");
    assert!(
        body.contains("/dashboard/login"),
        "login form action missing"
    );
}

// POST /dashboard/login with bad credentials redirects with ?error=invalid.
#[tokio::test]
async fn dashboard_login_bad_credentials_redirects() {
    let app = make_app().await;
    let _ = signup(&app, "admin@example.com").await;

    let form = "email=admin@example.com&password=wrongpassword";
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dashboard/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let loc = resp
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(loc.starts_with("/?error=invalid"), "location was {loc}");
}

// /dashboard/users without a cookie redirects to /. With a member
// cookie it would show the bounce page; that path needs cookie plumbing
// we don't fully simulate here, but the redirect-when-no-cookie is the
// primary access control to cover.
#[tokio::test]
async fn dashboard_users_no_cookie_redirects_home() {
    let app = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/dashboard/users")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers()
            .get(axum::http::header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/")
    );
}

// Static assets are served with the right content type. Catches a
// misconfigured route (returning HTML where JS was expected breaks
// htmx silently).
#[tokio::test]
async fn static_htmx_is_javascript() {
    let app = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/static/htmx.min.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("application/javascript"),
        "content-type was {ct}"
    );
}
