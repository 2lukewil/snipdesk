//! Prometheus `/metrics` endpoint for ops scraping.
//!
//! Disabled unless `metrics_token` is configured (env
//! `SNIPDESK_METRICS_TOKEN`): no token means the route 404s, so the
//! surface is opt-in. When enabled, scrapers authenticate with
//! `Authorization: Bearer <token>`. The endpoint sits on the main
//! listener; ops that want network isolation can also restrict it at
//! the proxy. Output is the Prometheus text exposition format.

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sqlx::SqlitePool;

use crate::http::AppState;

/// Best-effort scalar count; a failed query contributes 0 rather than
/// failing the whole scrape (a metrics endpoint should degrade, not
/// 500).
async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap_or(0)
}

async fn count_since(pool: &SqlitePool, sql: &str, ts: i64) -> i64 {
    sqlx::query_scalar(sql)
        .bind(ts)
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

pub async fn metrics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // No token configured -> the endpoint doesn't exist.
    let Some(expected) = state.metrics_token.as_deref() else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim);
    if provided != Some(expected) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized\n",
        )
            .into_response();
    }

    let pool = &state.pool;
    let now = chrono::Utc::now().timestamp();
    let day_ago = now - 86_400;
    let week_ago = now - 7 * 86_400;
    let month_ago = now - 30 * 86_400;

    let users_total = count(pool, "SELECT COUNT(*) FROM users").await;
    let users_active_24h = count_since(
        pool,
        "SELECT COUNT(*) FROM users WHERE last_seen_at >= ?",
        day_ago,
    )
    .await;
    let users_active_7d = count_since(
        pool,
        "SELECT COUNT(*) FROM users WHERE last_seen_at >= ?",
        week_ago,
    )
    .await;
    let users_active_30d = count_since(
        pool,
        "SELECT COUNT(*) FROM users WHERE last_seen_at >= ?",
        month_ago,
    )
    .await;
    let admins = count(pool, "SELECT COUNT(*) FROM users WHERE role = 'admin'").await;
    let library_snippets = count(
        pool,
        "SELECT COUNT(*) FROM library_snippets WHERE is_deleted = 0",
    )
    .await;
    // Library snippets that have never been pasted by anyone: a prune
    // signal, and the inverse of "active" content.
    let library_snippets_unused = count(
        pool,
        "SELECT COUNT(*) FROM library_snippets s WHERE s.is_deleted = 0 \
         AND NOT EXISTS (SELECT 1 FROM library_usage lu WHERE lu.snippet_id = s.id)",
    )
    .await;
    let library_folders = count(pool, "SELECT COUNT(*) FROM library_folders").await;
    let personal_snippets = count(
        pool,
        "SELECT COUNT(*) FROM personal_snippets WHERE is_deleted = 0",
    )
    .await;
    let pastes_total = count(pool, "SELECT COALESCE(SUM(snippets_pasted), 0) FROM users").await;
    let chars_pasted_total = count(pool, "SELECT COALESCE(SUM(chars_pasted), 0) FROM users").await;
    // Team-wide library paste volume, straight from the per-snippet
    // counters (distinct from snipdesk_pastes_total, which also counts
    // personal-snippet pastes).
    let library_pastes_total = count(
        pool,
        "SELECT COALESCE(SUM(usage_count), 0) FROM library_usage",
    )
    .await;
    let audit_events_total = count(pool, "SELECT COUNT(*) FROM audit_log").await;
    let version = env!("CARGO_PKG_VERSION");

    // Prometheus text exposition format. Each metric carries HELP +
    // TYPE so Grafana's explorer shows a description.
    let body = format!(
        "# HELP snipdesk_users_total Registered user accounts.\n\
         # TYPE snipdesk_users_total gauge\n\
         snipdesk_users_total {users_total}\n\
         # HELP snipdesk_users_active_24h Users seen in the last 24 hours.\n\
         # TYPE snipdesk_users_active_24h gauge\n\
         snipdesk_users_active_24h {users_active_24h}\n\
         # HELP snipdesk_users_active_7d Users seen in the last 7 days.\n\
         # TYPE snipdesk_users_active_7d gauge\n\
         snipdesk_users_active_7d {users_active_7d}\n\
         # HELP snipdesk_users_active_30d Users seen in the last 30 days.\n\
         # TYPE snipdesk_users_active_30d gauge\n\
         snipdesk_users_active_30d {users_active_30d}\n\
         # HELP snipdesk_admins_total Users with the admin role.\n\
         # TYPE snipdesk_admins_total gauge\n\
         snipdesk_admins_total {admins}\n\
         # HELP snipdesk_library_snippets_total Library snippets (excluding deleted).\n\
         # TYPE snipdesk_library_snippets_total gauge\n\
         snipdesk_library_snippets_total {library_snippets}\n\
         # HELP snipdesk_library_snippets_unused Library snippets never pasted.\n\
         # TYPE snipdesk_library_snippets_unused gauge\n\
         snipdesk_library_snippets_unused {library_snippets_unused}\n\
         # HELP snipdesk_library_folders_total Library folders.\n\
         # TYPE snipdesk_library_folders_total gauge\n\
         snipdesk_library_folders_total {library_folders}\n\
         # HELP snipdesk_personal_snippets_total Personal snippets (excluding deleted).\n\
         # TYPE snipdesk_personal_snippets_total gauge\n\
         snipdesk_personal_snippets_total {personal_snippets}\n\
         # HELP snipdesk_pastes_total Pastes reported by clients (cumulative).\n\
         # TYPE snipdesk_pastes_total counter\n\
         snipdesk_pastes_total {pastes_total}\n\
         # HELP snipdesk_library_pastes_total Library-snippet pastes, team-wide (cumulative).\n\
         # TYPE snipdesk_library_pastes_total counter\n\
         snipdesk_library_pastes_total {library_pastes_total}\n\
         # HELP snipdesk_chars_pasted_total Characters pasted by clients (cumulative).\n\
         # TYPE snipdesk_chars_pasted_total counter\n\
         snipdesk_chars_pasted_total {chars_pasted_total}\n\
         # HELP snipdesk_audit_events_total Audit-log entries recorded (cumulative).\n\
         # TYPE snipdesk_audit_events_total counter\n\
         snipdesk_audit_events_total {audit_events_total}\n\
         # HELP snipdesk_build_info Build version (value is always 1).\n\
         # TYPE snipdesk_build_info gauge\n\
         snipdesk_build_info{{version=\"{version}\"}} 1\n"
    );

    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}
