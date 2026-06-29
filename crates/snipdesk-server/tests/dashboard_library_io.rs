//! Integration tests for the dashboard library import/export flow.
//!
//! Covers the wire-visible contract: export downloads honour the
//! search filter and carry the interchange shape, the CSV header
//! includes folder_path, the import preview renders the tree with
//! duplicate badging, and the confirm step inserts only the
//! selected non-duplicate entries while writing the audit summary.

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
        oidc_allowed_redirect_urls: Vec::new(),
        secure_cookies: false,
        password_enabled: true,
        stats: snipdesk_server::config::StatsConfig::default(),
        fx_cache: Arc::new(snipdesk_server::fx::FxCache::default()),
        cors_allowed_origins: Vec::new(),
        brand_name: "SnipDesk".to_string(),
        metrics_token: None,
        ticket_link_enabled: false,
        ticket_url_pattern: None,
        update_cache: Arc::new(snipdesk_server::updater::UpdateCache::default()),
    };
    (pool, router(state))
}

/// Bootstrap the first admin via the setup form; returns the session
/// cookie value to attach to subsequent requests.
async fn admin_cookie(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dashboard/setup")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "display_name=Admin&email=admin%40example.com&password=longenough123",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .expect("setup should set a cookie");
    // "name=value; HttpOnly; ..." -> "name=value"
    set_cookie.split(';').next().unwrap().to_string()
}

async fn post_form_authed(
    app: &axum::Router,
    cookie: &str,
    path: &str,
    body: String,
) -> (StatusCode, Option<String>, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let loc = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, loc, String::from_utf8_lossy(&bytes).to_string())
}

async fn get_authed(
    app: &axum::Router,
    cookie: &str,
    path: &str,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&bytes).to_string())
}

/// Seed one library snippet through the dashboard create form.
async fn seed_snippet(app: &axum::Router, cookie: &str, title: &str, folder: &str) {
    let body = format!(
        "title={}&body=Hello+from+{}&tags=seeded&folder_path={}",
        urlencoding::encode(title),
        urlencoding::encode(title),
        urlencoding::encode(folder),
    );
    let (status, _, page) = post_form_authed(app, cookie, "/dashboard/library", body).await;
    assert_eq!(status, StatusCode::OK, "seed failed: {page}");
}

#[tokio::test]
async fn export_json_respects_search_filter() {
    let (_pool, app) = make_app().await;
    let cookie = admin_cookie(&app).await;
    seed_snippet(&app, &cookie, "Refund intro", "Billing").await;
    seed_snippet(&app, &cookie, "Greeting", "").await;

    let (status, headers, body) = get_authed(
        &app,
        &cookie,
        "/dashboard/library/export?format=json&q=refund",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let disp = headers
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(disp.starts_with("attachment"), "got disposition: {disp}");

    let entries: Vec<serde_json::Value> = serde_json::from_str(&body).expect("json body");
    assert_eq!(entries.len(), 1, "filter should narrow to one: {body}");
    assert_eq!(entries[0]["title"], "Refund intro");
    assert_eq!(entries[0]["folder_path"], "Billing");
}

#[tokio::test]
async fn export_csv_carries_folder_column() {
    let (_pool, app) = make_app().await;
    let cookie = admin_cookie(&app).await;
    seed_snippet(&app, &cookie, "Refund intro", "Billing").await;

    let (status, _, body) = get_authed(&app, &cookie, "/dashboard/library/export?format=csv").await;
    assert_eq!(status, StatusCode::OK);
    // CSV downloads lead with a UTF-8 BOM so Excel doesn't decode
    // them as ANSI; the header follows immediately after.
    assert!(body.starts_with('\u{feff}'), "csv export must carry a BOM");
    let header_line = body
        .lines()
        .next()
        .unwrap_or("")
        .trim_start_matches('\u{feff}');
    assert_eq!(header_line, "title,body,tags,folder_path");
    assert!(body.contains("Refund intro"));
    assert!(body.contains("Billing"));
}

#[tokio::test]
async fn export_selected_posts_exact_ids() {
    let (pool, app) = make_app().await;
    let cookie = admin_cookie(&app).await;
    seed_snippet(&app, &cookie, "Keep me", "Billing").await;
    seed_snippet(&app, &cookie, "Leave me", "").await;

    let keep_id: String =
        sqlx::query_scalar("SELECT id FROM library_snippets WHERE title = 'Keep me'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let selected = serde_json::to_string(&vec![keep_id]).unwrap();
    let (status, _, body) = post_form_authed(
        &app,
        &cookie,
        "/dashboard/library/export",
        format!("format=json&selected={}", urlencoding::encode(&selected)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries: Vec<serde_json::Value> = serde_json::from_str(&body).expect("json body");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["title"], "Keep me");
}

#[tokio::test]
async fn export_picker_renders_full_tree_selected() {
    let (_pool, app) = make_app().await;
    let cookie = admin_cookie(&app).await;
    seed_snippet(&app, &cookie, "Refund intro", "Billing").await;

    let (status, _, body) = get_authed(&app, &cookie, "/dashboard/library/export/picker").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("imp-tree"), "tree container missing");
    assert!(body.contains("checked"), "entries should start selected");
    assert!(
        body.contains("Export JSON") && body.contains("Export CSV"),
        "format buttons missing"
    );
}

#[tokio::test]
async fn export_requires_a_session() {
    let (_pool, app) = make_app().await;
    // Bootstrap an admin so the setup form isn't what answers /.
    let _cookie = admin_cookie(&app).await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/dashboard/library/export?format=json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER, "must bounce to login");
}

#[tokio::test]
async fn import_preview_renders_tree_and_flags_duplicates() {
    let (_pool, app) = make_app().await;
    let cookie = admin_cookie(&app).await;
    seed_snippet(&app, &cookie, "Greeting", "").await;

    let content = serde_json::json!([
        { "title": "Greeting", "body": "dup of the seeded one", "tags": [], "folder_path": null },
        { "title": "Refund steps", "body": "fresh", "tags": ["billing"], "folder_path": "Billing/Refunds" },
    ])
    .to_string();
    let (status, _, page) = post_form_authed(
        &app,
        &cookie,
        "/dashboard/library/import/preview",
        format!("content={}", urlencoding::encode(&content)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("imp-tree"), "tree container missing");
    assert!(page.contains("imp-badge"), "duplicate badge missing");
    assert!(page.contains("Billing"), "folder node missing");
    assert!(page.contains("Refunds"), "nested folder node missing");
    assert!(
        page.contains("name=\"payload\""),
        "hidden payload field missing"
    );
}

#[tokio::test]
async fn import_confirm_inserts_selected_and_audits() {
    let (pool, app) = make_app().await;
    let cookie = admin_cookie(&app).await;
    seed_snippet(&app, &cookie, "Greeting", "").await;

    let payload = serde_json::json!([
        { "title": "Refund steps", "body": "fresh", "tags": ["billing"], "folder_path": "Billing" },
        { "title": "Not selected", "body": "left behind", "tags": [], "folder_path": null },
        { "title": "Greeting", "body": "duplicate", "tags": [], "folder_path": null },
    ])
    .to_string();
    // Select index 0 (new) and 2 (duplicate); index 1 stays behind.
    let body = format!(
        "payload={}&selected={}",
        urlencoding::encode(&payload),
        urlencoding::encode("[0,2]"),
    );
    let (status, loc, _) = post_form_authed(&app, &cookie, "/dashboard/library/import", body).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        loc.as_deref(),
        Some("/dashboard/library?imported=1&skipped=1")
    );

    let titles: Vec<String> =
        sqlx::query_scalar("SELECT title FROM library_snippets WHERE is_deleted = 0")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(titles.iter().any(|t| t == "Refund steps"));
    assert!(
        !titles.iter().any(|t| t == "Not selected"),
        "unselected entry must not import"
    );
    assert_eq!(
        titles.iter().filter(|t| t.as_str() == "Greeting").count(),
        1,
        "duplicate must be skipped, not doubled"
    );

    let audit_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE action = 'library.import'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audit_count, 1, "import summary audit row missing");
}

#[tokio::test]
async fn folder_delete_confirm_counts_recursively() {
    let (_pool, app) = make_app().await;
    let cookie = admin_cookie(&app).await;
    seed_snippet(&app, &cookie, "Top level", "Billing").await;
    seed_snippet(&app, &cookie, "Nested", "Billing/Refunds").await;
    seed_snippet(&app, &cookie, "Elsewhere", "Other").await;

    let (status, _, page) = get_authed(
        &app,
        &cookie,
        "/dashboard/library/folders/delete/confirm?path=Billing",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("<strong>2</strong>"),
        "count must include subfolders: {page}"
    );
    assert!(page.contains("Move them to Unfiled"));
    assert!(page.contains("Delete them too"));
}

#[tokio::test]
async fn folder_delete_move_mode_unfiles_contents() {
    let (pool, app) = make_app().await;
    let cookie = admin_cookie(&app).await;
    seed_snippet(&app, &cookie, "Top level", "Billing").await;
    seed_snippet(&app, &cookie, "Nested", "Billing/Refunds").await;
    seed_snippet(&app, &cookie, "Elsewhere", "Other").await;

    let (status, loc, _) = post_form_authed(
        &app,
        &cookie,
        "/dashboard/library/folders/delete",
        "path=Billing&mode=move".to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        loc.as_deref(),
        Some("/dashboard/library?folder_deleted=Billing&moved=2")
    );

    let unfiled: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM library_snippets \
         WHERE is_deleted = 0 AND folder_path IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unfiled, 2, "both snippets should land in Unfiled");
    let elsewhere: Option<String> =
        sqlx::query_scalar("SELECT folder_path FROM library_snippets WHERE title = 'Elsewhere'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        elsewhere.as_deref(),
        Some("Other"),
        "other folders untouched"
    );
    let folder_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM library_folders WHERE path = 'Billing' OR path LIKE 'Billing/%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(folder_rows, 0, "folder rows must be gone");
    let audited: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE action = 'library.folder.delete'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audited, 1);
}

#[tokio::test]
async fn folder_delete_cascade_tombstones_with_version_bumps() {
    let (pool, app) = make_app().await;
    let cookie = admin_cookie(&app).await;
    seed_snippet(&app, &cookie, "Top level", "Billing").await;
    seed_snippet(&app, &cookie, "Nested", "Billing/Refunds").await;

    let max_before: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM library_snippets")
            .fetch_one(&pool)
            .await
            .unwrap();

    let (status, loc, _) = post_form_authed(
        &app,
        &cookie,
        "/dashboard/library/folders/delete",
        "path=Billing&mode=delete".to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        loc.as_deref(),
        Some("/dashboard/library?folder_deleted=Billing&deleted=2")
    );

    let live: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM library_snippets WHERE is_deleted = 0")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(live, 0, "both snippets tombstoned");
    // Version bumps put the tombstones in the sync stream so signed-in
    // clients see the deletions on their next incremental pull.
    let bumped: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM library_snippets WHERE is_deleted = 1 AND version > ?",
    )
    .bind(max_before)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bumped, 2, "tombstones must carry fresh versions");
}

#[tokio::test]
async fn folder_delete_rejects_sentinels() {
    let (_pool, app) = make_app().await;
    let cookie = admin_cookie(&app).await;
    for path in ["__all__", "__unfiled__", ""] {
        let (status, _, _) = post_form_authed(
            &app,
            &cookie,
            "/dashboard/library/folders/delete",
            format!("path={path}&mode=move"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "path {path:?} must be rejected"
        );
    }
}
