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

    let users_total = count(pool, "SELECT COUNT(*) FROM users").await;
    let users_active_24h = count_since(
        pool,
        "SELECT COUNT(*) FROM users WHERE last_seen_at >= ?",
        day_ago,
    )
    .await;
    let admins = count(pool, "SELECT COUNT(*) FROM users WHERE role = 'admin'").await;
    let library_snippets = count(
        pool,
        "SELECT COUNT(*) FROM library_snippets WHERE is_deleted = 0",
    )
    .await;
    let personal_snippets = count(
        pool,
        "SELECT COUNT(*) FROM personal_snippets WHERE is_deleted = 0",
    )
    .await;
    let pastes_total = count(pool, "SELECT COALESCE(SUM(snippets_pasted), 0) FROM users").await;
    let chars_pasted_total = count(pool, "SELECT COALESCE(SUM(chars_pasted), 0) FROM users").await;

    // Prometheus text exposition format. Each metric carries HELP +
    // TYPE so Grafana's explorer shows a description.
    let body = format!(
        "# HELP snipdesk_users_total Registered user accounts.\n\
         # TYPE snipdesk_users_total gauge\n\
         snipdesk_users_total {users_total}\n\
         # HELP snipdesk_users_active_24h Users seen in the last 24 hours.\n\
         # TYPE snipdesk_users_active_24h gauge\n\
         snipdesk_users_active_24h {users_active_24h}\n\
         # HELP snipdesk_admins_total Users with the admin role.\n\
         # TYPE snipdesk_admins_total gauge\n\
         snipdesk_admins_total {admins}\n\
         # HELP snipdesk_library_snippets_total Library snippets (excluding deleted).\n\
         # TYPE snipdesk_library_snippets_total gauge\n\
         snipdesk_library_snippets_total {library_snippets}\n\
         # HELP snipdesk_personal_snippets_total Personal snippets (excluding deleted).\n\
         # TYPE snipdesk_personal_snippets_total gauge\n\
         snipdesk_personal_snippets_total {personal_snippets}\n\
         # HELP snipdesk_pastes_total Pastes reported by clients (cumulative).\n\
         # TYPE snipdesk_pastes_total counter\n\
         snipdesk_pastes_total {pastes_total}\n\
         # HELP snipdesk_chars_pasted_total Characters pasted by clients (cumulative).\n\
         # TYPE snipdesk_chars_pasted_total counter\n\
         snipdesk_chars_pasted_total {chars_pasted_total}\n"
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
