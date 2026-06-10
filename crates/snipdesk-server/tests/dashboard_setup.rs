//! Integration tests for the first-run dashboard setup flow.
//!
//! A fresh database renders a create-first-admin form at `/`; the
//! POST creates the account atomically (racing submits can't both
//! land) and signs the new admin straight in via the session cookie.
//! Once any account exists, `/` goes back to the normal login form
//! and the setup POST permanently no-ops.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use snipdesk_server::config::MasterKey;
use snipdesk_server::db;
use snipdesk_server::http::{router, AppState};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;
use tower::ServiceExt;

async fn make_app() -> (SqlitePool, axum::Router) {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    db::run_migrations(&pool).await.expect("migrations");
    let state = AppState {
        pool: pool.clone(),
        master_key: Arc::new(MasterKey::generate()),
        jwt_secret: "test-jwt-secret-not-for-production".into(),
        oidc_google: None,
        oidc_keycloak: None,
        oidc_allowed_schemes: vec!["snipdesk".to_string()],
        secure_cookies: false,
        stats: snipdesk_server::config::StatsConfig::default(),
        fx_cache: Arc::new(snipdesk_server::fx::FxCache::default()),
        cors_allowed_origins: Vec::new(),
        brand_name: "SnipDesk".to_string(),
        update_cache: Arc::new(snipdesk_server::updater::UpdateCache::default()),
    };
    (pool, router(state))
}

async fn get_page(app: &axum::Router, path: &str) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("oneshot");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// POST a form body; return (status, Location header, Set-Cookie header).
async fn post_form(
    app: &axum::Router,
    path: &str,
    body: &str,
) -> (StatusCode, Option<String>, Option<String>) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body.to_string()))
                .expect("build request"),
        )
        .await
        .expect("oneshot");
    let status = resp.status();
    let loc = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    (status, loc, cookie)
}

async fn user_count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await
        .expect("count")
}

#[tokio::test]
async fn fresh_db_renders_setup_form_at_root() {
    let (_pool, app) = make_app().await;
    let (status, body) = get_page(&app, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("/dashboard/setup"),
        "expected setup form, got: {body}"
    );
    assert!(
        !body.contains("/dashboard/login"),
        "login form should not render while no accounts exist"
    );
}

#[tokio::test]
async fn setup_creates_admin_and_signs_in() {
    let (pool, app) = make_app().await;
    let (status, loc, cookie) = post_form(
        &app,
        "/dashboard/setup",
        "display_name=Op&email=op%40example.com&password=longenough123",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(loc.as_deref(), Some("/dashboard/users"));
    assert!(
        cookie.unwrap_or_default().contains("snipdesk_dashboard="),
        "setup should set the session cookie"
    );

    let (count, role): (i64, String) = sqlx::query_as("SELECT COUNT(*), MAX(role) FROM users")
        .fetch_one(&pool)
        .await
        .expect("read back");
    assert_eq!(count, 1);
    assert_eq!(role, "admin");

    // An audit row records the bootstrap.
    let audited: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE action = 'user.create'")
            .fetch_one(&pool)
            .await
            .expect("audit count");
    assert_eq!(audited, 1);
}

#[tokio::test]
async fn second_setup_attempt_is_rejected() {
    let (pool, app) = make_app().await;
    let _ = post_form(
        &app,
        "/dashboard/setup",
        "display_name=First&email=first%40example.com&password=longenough123",
    )
    .await;
    let (status, loc, cookie) = post_form(
        &app,
        "/dashboard/setup",
        "display_name=Second&email=second%40example.com&password=longenough123",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(loc.as_deref(), Some("/"), "loser should bounce to login");
    assert!(cookie.is_none(), "no session for the rejected attempt");
    assert_eq!(user_count(&pool).await, 1);
}

#[tokio::test]
async fn weak_password_bounces_without_creating() {
    let (pool, app) = make_app().await;
    let (status, loc, _) = post_form(
        &app,
        "/dashboard/setup",
        "display_name=Op&email=op%40example.com&password=short",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(loc.as_deref(), Some("/?error=setup_password"));
    assert_eq!(user_count(&pool).await, 0);
}

#[tokio::test]
async fn root_shows_login_once_an_account_exists() {
    let (_pool, app) = make_app().await;
    let _ = post_form(
        &app,
        "/dashboard/setup",
        "display_name=Op&email=op%40example.com&password=longenough123",
    )
    .await;
    let (status, body) = get_page(&app, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("/dashboard/login"),
        "expected login form after setup, got: {body}"
    );
    assert!(
        !body.contains("/dashboard/setup"),
        "setup form should be gone once an account exists"
    );
}
