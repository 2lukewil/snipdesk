//! Axum router. Currently exposes only `/api/health` - every other
//! endpoint lands in this file as later phases add them (auth, snippet
//! sync, library, admin).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use once_cell::sync::Lazy;
use serde::Serialize;
use sqlx::SqlitePool;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::config::{GoogleOidcConfig, KeycloakOidcConfig, MasterKey, StatsConfig};
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
    /// provider isn't configured; per-provider OIDC endpoints check
    /// the matching field and return a clean "not configured" error
    /// when their slot is None.
    pub oidc_google: Option<GoogleOidcConfig>,
    /// Keycloak OIDC config when set in `[oidc.keycloak]`. Independent
    /// of Google; both, either, or neither can be configured.
    pub oidc_keycloak: Option<KeycloakOidcConfig>,
    /// Deep-link URL schemes the OIDC start endpoint will trust in
    /// the `?redirect=<scheme>://auth` parameter. Mirrors
    /// `[oidc].allowed_deep_link_schemes` from the TOML; a
    /// whitelabel-aware server lists every brand it serves so the
    /// allowlist accepts each one.
    pub oidc_allowed_schemes: Vec<String>,
    /// Full redirect URLs accepted in `?redirect=` (exact match). The
    /// browser extension's `https://<id>.chromiumapp.org/` lives here;
    /// keeps the https redirect from being an open redirector.
    pub oidc_allowed_redirect_urls: Vec<String>,
    /// Dashboard session cookie gets the `Secure` attribute when this
    /// is `true`. Forwarded from `secure_cookies` in the TOML config.
    pub secure_cookies: bool,
    /// Email/password auth master switch (config `password_enabled`,
    /// env SNIPDESK_PASSWORD_ENABLED). When false the deployment is
    /// SSO-only: the password endpoints reject server-side and every
    /// sign-in surface renders only the configured OIDC providers.
    pub password_enabled: bool,
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
    /// Origins that may make cross-origin JSON-API requests. Empty
    /// (default) means no CORS layer is mounted at all. Read by
    /// `router()` at construction; not used by individual handlers.
    pub cors_allowed_origins: Vec<String>,
    /// Brand name surfaced in the dashboard chrome (browser title,
    /// nav header, login card, member-blocked page). Defaults to
    /// "SnipDesk" via BrandConfig; deployments override via
    /// `[brand].name` in the server TOML.
    pub brand_name: String,
    /// Latest known server release vs. running version. Updated by
    /// the background poller in `crate::updater`; the dashboard
    /// renders a banner when `is_newer` flips true.
    pub update_cache: Arc<crate::updater::UpdateCache>,
}

/// Per-route body caps, sized to the realistic payload for each
/// endpoint. The router defaults to the tight cap (BODY_LIMIT_SMALL);
/// routes that carry user content (snippets, library, dashboard
/// forms) override upward via `.route_layer(DefaultBodyLimit::max(...))`.
///
/// Single global cap was the v1.0 default - one number was simpler -
/// but it meant a 2 MiB request could land on /api/me (which accepts
/// at most ~50 bytes of JSON). Per-route caps shrink the blast radius
/// of a misbehaved client or an attacker probing the surface.
const BODY_LIMIT_SMALL: usize = 32 * 1024; // 32 KiB - auth, admin mutations, /api/me
const BODY_LIMIT_MEDIUM: usize = 256 * 1024; // 256 KiB - usage telemetry batches
/// 2 MiB - snippet + library bodies. Public so the dashboard module
/// can reuse the same constant for its library-form posts.
pub const BODY_LIMIT_LARGE: usize = 2 * 1024 * 1024;

pub fn router(state: AppState) -> Router {
    // Build the CORS layer when origins are configured; otherwise
    // skip mounting CORS entirely. The default empty list matches
    // v1 behaviour (same-origin only).
    let cors_layer = build_cors_layer(&state.cors_allowed_origins);
    let mut router = build_inner_router();
    if let Some(layer) = cors_layer {
        router = router.layer(layer);
    }
    router
        .layer(TraceLayer::new_for_http())
        // Global default for any route that didn't opt up. Tight by
        // intent: most endpoints (auth, /api/me, admin user updates)
        // accept tiny JSON payloads. The big-content routes
        // (snippets, library, dashboard form posts) override upward
        // via `.layer(DefaultBodyLimit::max(BODY_LIMIT_LARGE))` per
        // route. A new endpoint added without an explicit override
        // inherits this floor - fail-tight rather than fail-loose.
        .layer(DefaultBodyLimit::max(BODY_LIMIT_SMALL))
        .with_state(state)
}

/// Build the CORS layer if any origins are configured. Returns
/// `None` when the list is empty - no layer is mounted at all in
/// that case, preserving the v1 same-origin-only behaviour.
///
/// Invalid origin strings are dropped with a warn log rather than
/// rejected. Mis-typing one origin shouldn't take down the whole
/// CORS configuration.
fn build_cors_layer(origins: &[String]) -> Option<CorsLayer> {
    if origins.is_empty() {
        return None;
    }
    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| match HeaderValue::from_str(o) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("cors: ignoring invalid origin {o:?}: {e}");
                None
            }
        })
        .collect();
    if parsed.is_empty() {
        tracing::warn!("cors: all configured origins failed to parse; CORS layer not mounted");
        return None;
    }
    tracing::info!(origins = parsed.len(), "cors: layer enabled");
    Some(
        CorsLayer::new()
            .allow_origin(parsed)
            .allow_methods(tower_http::cors::AllowMethods::any())
            .allow_headers(tower_http::cors::AllowHeaders::any())
            .allow_credentials(true),
    )
}

fn build_inner_router() -> Router<AppState> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/auth/methods", get(handlers::auth::methods))
        .route(
            "/api/auth/signup",
            post(handlers::auth::signup).layer(middleware::from_fn(auth_rate_limit)),
        )
        .route(
            "/api/auth/login",
            post(handlers::auth::login).layer(middleware::from_fn(auth_rate_limit)),
        )
        .route("/api/auth/logout", post(handlers::auth::logout))
        .route(
            "/api/me",
            get(handlers::auth::me).patch(handlers::auth::update_me),
        )
        // OIDC. Both endpoints are public (no AuthUser required);
        // the start endpoint initiates the OAuth dance and the
        // callback validates the IdP's response. The per-provider
        // routes (`/api/auth/oidc/:provider/{start,callback}`) are
        // the canonical surface as of Keycloak step 4; the legacy
        // unscoped routes stay mounted as Google shims so older
        // client builds keep working without a forced upgrade.
        .route("/api/auth/oidc/start", get(handlers::oidc::start))
        .route("/api/auth/oidc/callback", get(handlers::oidc::callback))
        .route(
            "/api/auth/oidc/:provider/start",
            get(handlers::oidc::start_provider),
        )
        .route(
            "/api/auth/oidc/:provider/callback",
            get(handlers::oidc::callback_provider),
        )
        .route(
            "/api/snippets",
            post(handlers::snippets::create)
                .get(handlers::snippets::list)
                .layer(DefaultBodyLimit::max(BODY_LIMIT_LARGE)),
        )
        .route(
            "/api/snippets/:id",
            put(handlers::snippets::update)
                .delete(handlers::snippets::delete)
                .layer(DefaultBodyLimit::max(BODY_LIMIT_LARGE)),
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
            post(handlers::library::create)
                .get(handlers::library::list)
                .layer(DefaultBodyLimit::max(BODY_LIMIT_LARGE)),
        )
        .route(
            "/api/library/:id",
            put(handlers::library::update)
                .delete(handlers::library::delete)
                .layer(DefaultBodyLimit::max(BODY_LIMIT_LARGE)),
        )
        // Paste telemetry. Auth-required; the desktop client posts
        // delta packets every sync tick. Folded into the per-user +
        // per-snippet counters used by the stats page and library
        // page. Medium cap fits a batch of a few hundred deltas
        // without being a 2 MiB DoS surface.
        .route(
            "/api/usage/report",
            post(handlers::usage::report).layer(DefaultBodyLimit::max(BODY_LIMIT_MEDIUM)),
        )
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
}

// --- Per-client rate limit for the auth endpoints ---
// Fixed-window counter keyed by the caller's IP, read from
// X-Forwarded-For / X-Real-IP since the server runs behind a
// TLS-terminating proxy (falls back to one shared bucket when neither
// header is present). Brute-force defence-in-depth on the public,
// unauthenticated surface; the cap is generous so normal sign-in is
// never affected.
const AUTH_RL_WINDOW: Duration = Duration::from_secs(60);
const AUTH_RL_MAX: u32 = 20;

static AUTH_RL: Lazy<Mutex<HashMap<String, (u32, Instant)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

async fn auth_rate_limit(req: Request, next: Next) -> Response {
    let key = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .or_else(|| {
            req.headers()
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "shared".to_string());

    let now = Instant::now();
    let over = {
        let mut map = AUTH_RL.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > 4096 {
            map.retain(|_, (_, start)| now.duration_since(*start) <= AUTH_RL_WINDOW);
        }
        let entry = map.entry(key).or_insert((0, now));
        if now.duration_since(entry.1) > AUTH_RL_WINDOW {
            *entry = (0, now);
        }
        entry.0 += 1;
        entry.0 > AUTH_RL_MAX
    };
    if over {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts; please wait a minute and try again",
        )
            .into_response();
    }
    next.run(req).await
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
