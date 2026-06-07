//! Axum router. Currently exposes only `/api/health` - every other
//! endpoint lands in this file as later phases add them (auth, snippet
//! sync, library, admin).

use std::sync::Arc;

use axum::{
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put},
    Json, Router,
};
use serde::Serialize;
use sqlx::SqlitePool;
use tower_http::trace::TraceLayer;

use crate::config::{GoogleOidcConfig, MasterKey, StatsConfig};
use crate::fx::FxCache;
use crate::handlers;

/// Shared application state. Cloned per handler invocation; `pool` and
/// `master_key` are wrapped in Arc so the clones are cheap.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    /// Server-side master key used by `crypto` to encrypt/decrypt
    /// personal-snippet payloads on insert/read.
    pub master_key: Arc<MasterKey>,
    /// HS256 secret for signing/verifying JWTs. Loaded from config at
    /// startup; empty when no auth is configured (handlers will reject
    /// then, but having the field always present keeps state lean).
    pub jwt_secret: String,
    /// Google OIDC config when set in `[oidc.google]`. None when this
    /// server is in password-only mode; the OIDC endpoints return a
    /// "not configured" error instead of 500ing.
    pub oidc_google: Option<GoogleOidcConfig>,
    /// Dashboard session cookie gets the `Secure` attribute when this
    /// is `true`. Forwarded from `secure_cookies` in the TOML config.
    pub secure_cookies: bool,
    /// Stats-page knobs (wpm / wage / currency conversion). Cheap
    /// to clone (small map of FX rates) - we pass by value rather
    /// than Arc-wrap.
    pub stats: StatsConfig,
    /// Live FX cache. Always present; empty by default. Populated
    /// by `crate::fx::spawn_refresher` when `[fx]` is configured in
    /// the TOML. The dashboard reads via `crate::fx::rate_for`
    /// which falls through to `stats.aud_rates` when the live
    /// cache misses a code.
    pub fx_cache: Arc<FxCache>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/auth/signup", post(handlers::auth::signup))
        .route("/api/auth/login", post(handlers::auth::login))
        .route("/api/auth/logout", post(handlers::auth::logout))
        .route(
            "/api/me",
            get(handlers::auth::me).patch(handlers::auth::update_me),
        )
        // OIDC / "Sign in with Google" - both endpoints are public
        // (no AuthUser required); the start endpoint initiates the
        // OAuth dance and the callback validates the response from
        // Google. Returns "oidc_disabled" 400 when [oidc.google]
        // isn't configured on this server.
        .route("/api/auth/oidc/start", get(handlers::oidc::start))
        .route("/api/auth/oidc/callback", get(handlers::oidc::callback))
        .route(
            "/api/snippets",
            post(handlers::snippets::create).get(handlers::snippets::list),
        )
        .route(
            "/api/snippets/:id",
            put(handlers::snippets::update).delete(handlers::snippets::delete),
        )
        // Trash view + restore: the dashboard / desktop client read
        // these to show soft-deleted snippets and bring them back.
        // `trash` and the `restore` POST are kept off the
        // /api/snippets/:id namespace because the URL `/restore`
        // collides with a hypothetical snippet id of "restore"; the
        // dedicated path is clearer.
        .route("/api/snippets/trash", get(handlers::snippets::trash))
        .route(
            "/api/snippets/:id/restore",
            post(handlers::snippets::restore),
        )
        // Shared team library - GET is open to any signed-in member; the
        // write handlers gate on `auth.require_admin()` internally.
        .route(
            "/api/library",
            post(handlers::library::create).get(handlers::library::list),
        )
        .route(
            "/api/library/:id",
            put(handlers::library::update).delete(handlers::library::delete),
        )
        // Paste telemetry. Auth-required; the desktop client posts
        // delta packets every sync tick. Folded into the per-user +
        // per-snippet counters used by the stats page and library
        // page.
        .route("/api/usage/report", post(handlers::usage::report))
        // Admin user management - JSON API; the htmx dashboard uses
        // these handlers directly (not over HTTP) but mounting them
        // here keeps a single source of truth and exposes the surface
        // for future CLI / external admin tooling.
        .route("/api/admin/users", get(handlers::admin::list_users))
        .route(
            "/api/admin/users/:id",
            put(handlers::admin::update_user).delete(handlers::admin::delete_user),
        )
        // Server-rendered htmx dashboard (phase 6+). Sits on the same
        // listener so a single binary serves both the JSON API and the
        // admin UI. Cookie-gated; non-admins see a bounce page.
        .merge(crate::dashboard::routes())
        .layer(TraceLayer::new_for_http())
        // 2 MiB body cap. Snippet bodies in practice run to a few KB;
        // anything past this is either a misuse (someone POSTing a
        // large file body) or an attempted DoS. Set at the router
        // level so it covers JSON API + dashboard form posts alike.
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(state)
}

#[derive(Serialize)]
struct HealthResponse {
    /// Always "ok" when this handler responds at all.
    status: &'static str,
    /// CARGO_PKG_VERSION baked in at compile time.
    version: &'static str,
    /// Tells us whether the DB pool is reachable; if the SQLite file is
    /// missing or corrupt this flips to false.
    db: bool,
}

/// Probe endpoint for load balancers / docker healthchecks. Returns
/// 200 OK when the DB ping succeeds, 503 Service Unavailable when it
/// doesn't, so an orchestrator sees a dead server as unhealthy and
/// stops routing traffic. Body is JSON either way so a curl-based
/// check can inspect details.
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let db_ok = sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&state.pool)
        .await
        .map(|n| n == 1)
        .unwrap_or(false);
    let status = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(HealthResponse {
            status: if db_ok { "ok" } else { "degraded" },
            version: env!("CARGO_PKG_VERSION"),
            db: db_ok,
        }),
    )
}
