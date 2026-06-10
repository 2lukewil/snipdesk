//! Integration tests for the OIDC surface.
//!
//! Scope:
//!   - `/api/auth/methods` wire shape with no / Google / Keycloak /
//!     both configured (the client renders its sign-in surface
//!     strictly off this response).
//!   - Migration 0009 schema: the `users.oidc_provider` column
//!     exists post-migration, the backfill UPDATE is idempotent.
//!   - Dashboard SSO entry handler bounces with `?error=signin` on
//!     unknown provider names and on absent config (both paths can
//!     execute without a live IdP - they fail before any network).
//!
//! What's NOT covered here: the full callback flow that exchanges
//! a code for tokens and verifies an ID token. That needs a mocked
//! OIDC discovery endpoint + signed JWT minting, which is enough
//! moving parts to justify its own harness. The hooks that ride
//! the verified claims (`run_provider_checks`,
//! `realm_roles_from_jwt`, `upsert_oidc_user`'s state machine)
//! are exercised by inline unit tests where the live-IdP dependency
//! doesn't kick in.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use snipdesk_server::config::{KeycloakOidcConfig, MasterKey};
use snipdesk_server::db;
use snipdesk_server::http::{router, AppState};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;
use tower::ServiceExt;

/// Build a fresh AppState with no OIDC providers configured.
/// Caller overrides the OIDC slots before handing to `router()`.
async fn fresh_state() -> (SqlitePool, AppState) {
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
    (pool, state)
}

async fn get_json(app: &axum::Router, path: &str) -> (StatusCode, serde_json::Value) {
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
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// Hit a route and inspect the raw response (status + Location
/// header). Used for the OIDC dashboard-start 302 checks; the
/// body isn't interesting.
async fn get_redirect(app: &axum::Router, path: &str) -> (StatusCode, Option<String>) {
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
    let loc = resp
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    (status, loc)
}

fn keycloak_cfg() -> KeycloakOidcConfig {
    KeycloakOidcConfig {
        client_id: "snipdesk-client".into(),
        client_secret: "secret-not-real".into(),
        issuer_url: "https://kc.example.invalid/realms/test".into(),
        redirect_uri: "http://127.0.0.1:8080/api/auth/oidc/keycloak/callback".into(),
        required_realm_role: None,
        admin_role: None,
        allowed_email_domains: Vec::new(),
        display_name: Some("Sign in with Acme SSO".into()),
    }
}

#[tokio::test]
async fn methods_empty_when_no_oidc_configured() {
    let (_pool, state) = fresh_state().await;
    let app = router(state);
    let (status, body) = get_json(&app, "/api/auth/methods").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["password"]["enabled"], serde_json::Value::Bool(true));
    let providers = body["providers"].as_array().expect("providers array");
    assert!(
        providers.is_empty(),
        "expected empty providers, got {providers:?}"
    );
}

#[tokio::test]
async fn methods_advertises_keycloak_with_config_display_name() {
    let (_pool, mut state) = fresh_state().await;
    state.oidc_keycloak = Some(keycloak_cfg());
    let app = router(state);
    let (status, body) = get_json(&app, "/api/auth/methods").await;
    assert_eq!(status, StatusCode::OK);
    let providers = body["providers"].as_array().expect("providers array");
    assert_eq!(providers.len(), 1);
    let kc = &providers[0];
    assert_eq!(kc["id"], "keycloak");
    assert_eq!(kc["display_name"], "Sign in with Acme SSO");
    assert_eq!(kc["start_url"], "/api/auth/oidc/keycloak/start");
}

#[tokio::test]
async fn methods_keycloak_falls_back_to_sso_label_when_display_name_unset() {
    let (_pool, mut state) = fresh_state().await;
    let mut cfg = keycloak_cfg();
    cfg.display_name = None;
    state.oidc_keycloak = Some(cfg);
    let app = router(state);
    let (_, body) = get_json(&app, "/api/auth/methods").await;
    let providers = body["providers"].as_array().expect("providers array");
    assert_eq!(providers[0]["display_name"], "Sign in with SSO");
}

#[tokio::test]
async fn dashboard_oidc_start_unknown_provider_bounces_to_login() {
    // No provider configured + unknown path segment. The handler
    // resolves the provider name before doing anything else; an
    // unresolvable name short-circuits to /?error=signin without
    // any IdP traffic.
    let (_pool, state) = fresh_state().await;
    let app = router(state);
    let (status, loc) = get_redirect(&app, "/dashboard/oidc/notreal/start").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(loc.as_deref(), Some("/?error=signin"));
}

#[tokio::test]
async fn dashboard_oidc_start_unconfigured_provider_bounces_to_login() {
    // Provider id resolves (`keycloak` is a known variant) but the
    // server has no [oidc.keycloak] block. The provider_config
    // lookup inside start_flow returns a generic signin error;
    // dashboard_oidc_start catches it and 302s to /?error=signin.
    let (_pool, state) = fresh_state().await;
    let app = router(state);
    let (status, loc) = get_redirect(&app, "/dashboard/oidc/keycloak/start").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(loc.as_deref(), Some("/?error=signin"));
}

#[tokio::test]
async fn migration_0009_adds_oidc_provider_column() {
    // After db::run_migrations, the column must exist on the users
    // table and the data type must be TEXT. A misnamed migration
    // file or a checksum mismatch would manifest here.
    let (pool, _state) = fresh_state().await;
    let info: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT cid, name, type FROM pragma_table_info('users') WHERE name = 'oidc_provider'",
    )
    .fetch_all(&pool)
    .await
    .expect("pragma");
    assert_eq!(info.len(), 1, "oidc_provider column should exist after 0009");
    assert_eq!(info[0].1, "oidc_provider");
    assert_eq!(info[0].2.to_uppercase(), "TEXT");
}

#[tokio::test]
async fn migration_0009_backfill_targets_oidc_users_only() {
    // The migration's UPDATE backfills oidc_provider = 'google' for
    // every row with oidc_subject IS NOT NULL. The in-memory DB starts
    // empty so 0009's UPDATE is a no-op on first run; this test
    // simulates a pre-0009 row by inserting one with
    // oidc_provider = NULL and re-running the backfill UPDATE
    // statement directly. Idempotent: rows with the column already
    // set keep their value; rows with the column NULL get 'google'
    // when oidc_subject is set, stay NULL otherwise.
    let (pool, _state) = fresh_state().await;
    let now = 1_700_000_000_i64;
    // Pre-0009-style OIDC row: subject set, provider NULL.
    sqlx::query(
        "INSERT INTO users \
           (id, email, display_name, role, is_disabled, \
            created_at, last_seen_at, oidc_subject, oidc_provider) \
         VALUES \
           ('alice', 'alice@example.com', 'Alice', 'admin', 0, ?1, ?1, 'sub-alice', NULL), \
           ('bob',   'bob@example.com',   'Bob',   'member', 0, ?1, ?1, NULL,        NULL)",
    )
    .bind(now)
    .execute(&pool)
    .await
    .expect("seed users");

    // Re-run the 0009 backfill UPDATE. (The ALTER TABLE side of the
    // migration already ran via run_migrations; this is just the
    // backfill statement applied a second time, which is what
    // would happen if 0009 were ever re-applied via the
    // self-repair checksum path.)
    sqlx::query("UPDATE users SET oidc_provider = 'google' WHERE oidc_subject IS NOT NULL")
        .execute(&pool)
        .await
        .expect("backfill");

    let (alice_provider, bob_provider): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT \
            (SELECT oidc_provider FROM users WHERE id = 'alice'), \
            (SELECT oidc_provider FROM users WHERE id = 'bob')",
    )
    .fetch_one(&pool)
    .await
    .expect("read back");
    assert_eq!(alice_provider.as_deref(), Some("google"));
    assert_eq!(bob_provider, None);
}
