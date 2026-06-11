//! Cookie-based session for the dashboard.
//!
//! We re-issue the same HS256 JWT the JSON API uses; the cookie is just
//! a different transport. That means a single sign-out from the
//! dashboard doesn't invalidate any desktop client tokens (and vice
//! versa) - they're independent sessions sharing the same signing key.
//! When we add a revocation list (v1.1) it would invalidate both.
//!
//! Cookie attributes:
//!   - `HttpOnly`: blocks JS access. The page only needs the cookie
//!     attached automatically on requests; it never reads it.
//!   - `SameSite=Lax`: blocks cross-site POSTs. We don't accept any
//!     dashboard mutation from a third-party origin so this is safe.
//!   - `Path=/`: required so /static/* requests carry the cookie too -
//!     not strictly necessary for static asset auth, but it means the
//!     cookie behaves uniformly across the whole dashboard.
//!   - `Secure`: omitted in v1 so localhost smoke tests work. In
//!     production the reverse proxy should refuse plaintext; v1.1
//!     should look at `X-Forwarded-Proto` and flip Secure on when the
//!     request reached us over HTTPS.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};

use crate::auth::{verify_token, Claims};
use crate::http::AppState;

pub const COOKIE_NAME: &str = "snipdesk_dashboard";

/// Build a Set-Cookie carrying the JWT, used after a successful login.
/// `secure` controls the `Secure` attribute - true in production
/// (config `secure_cookies = true`), false for localhost dev so HTTP
/// testing still works.
pub fn build_cookie(token: String, secure: bool) -> Cookie<'static> {
    Cookie::build((COOKIE_NAME, token))
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(secure)
        .path("/")
        // No max-age: the cookie expires with the browser session. The
        // JWT inside has its own 24h TTL; we honor whichever expires
        // first. Persistent cookies would mean a forgotten browser tab
        // stays admin-authenticated for the full JWT lifetime - wrong
        // default for an admin tool.
        .build()
}

/// A removal cookie - same name/path/HttpOnly/SameSite as the issued
/// cookie, with `max_age` zeroed and an expiry in the past, so the
/// browser drops it instead of just shadowing it. `make_removal()`
/// handles both knobs in one call.
pub fn clear_cookie() -> Cookie<'static> {
    let mut c = Cookie::build((COOKIE_NAME, ""))
        .http_only(true)
        .same_site(SameSite::Lax)
        .path("/")
        .build();
    c.make_removal();
    c
}

/// Authenticated dashboard session - extracted from the cookie jar.
/// Holds the JWT claims; admin enforcement is layered on top via
/// `DashboardAdmin` below.
pub struct DashboardSession {
    pub claims: Claims,
}

/// Auth-failure response that knows about htmx. Plain navigations
/// get a 303; htmx fragment requests (HX-Request header) get an
/// HX-Redirect, which tells htmx to navigate the WHOLE page there.
/// Without this, a dead session (server restart, secret rotation,
/// expiry) made every polling fragment FOLLOW the redirect and swap
/// the login or first-run page INTO the element it was refreshing -
/// e.g. the setup page rendered inside the users table.
fn auth_redirect(parts: &Parts, to: &str) -> Response {
    let is_htmx = parts
        .headers
        .get("hx-request")
        .map(|v| v == "true")
        .unwrap_or(false);
    if is_htmx {
        (
            axum::http::StatusCode::OK,
            [(
                axum::http::HeaderName::from_static("hx-redirect"),
                to.to_string(),
            )],
        )
            .into_response()
    } else {
        Redirect::to(to).into_response()
    }
}

#[axum::async_trait]
impl FromRequestParts<AppState> for DashboardSession {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_headers(&parts.headers);
        let token = jar
            .get(COOKIE_NAME)
            .map(|c| c.value().to_string())
            .ok_or_else(|| auth_redirect(parts, "/"))?;
        let claims = verify_token(&token, &state.jwt_secret)
            .map_err(|_| auth_redirect(parts, "/?expired=1"))?;
        Ok(Self { claims })
    }
}

/// Extractor that further enforces `role = "admin"`. Anything mounted at
/// `/dashboard/*` (except the login + logout routes) goes through this;
/// members hitting an admin page see a "members can't access" page
/// rather than a bare 403.
pub struct DashboardAdmin {
    pub claims: Claims,
}

impl DashboardAdmin {
    pub fn user_id(&self) -> &str {
        &self.claims.sub
    }
}

#[axum::async_trait]
impl FromRequestParts<AppState> for DashboardAdmin {
    /// Two failure shapes: redirect (not signed in) and a 403 HTML page
    /// (signed in but not admin). Both are Response so we don't have to
    /// thread an enum through every handler.
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let session = DashboardSession::from_request_parts(parts, state)
            .await
            .map_err(|r| r.into_response())?;
        if session.claims.role != "admin" {
            return Err(member_blocked_page(&state.brand_name).into_response());
        }
        Ok(Self {
            claims: session.claims,
        })
    }
}

/// Substitute the brand placeholder into the static template body.
/// Two replacements per page render is cheap and saves wiring an
/// HTML escaper for a string we already validated as a config-time
/// value.
fn member_blocked_page(brand_name: &str) -> impl IntoResponse {
    let body = include_str!("templates/member_blocked.html").replace("{{BRAND_NAME}}", brand_name);
    (
        StatusCode::FORBIDDEN,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
}

/// Pull out the `redirect_to` query param so post-login we send the
/// user back where they came from. Capped to a same-host path to avoid
/// an open-redirect: a leading double-slash (`//evil.example/...`) is
/// protocol-relative and would land them off-host.
pub fn safe_next(raw: Option<&str>) -> String {
    let raw = raw.unwrap_or("/dashboard/users").trim();
    if !raw.starts_with('/') || raw.starts_with("//") {
        return "/dashboard/users".to_string();
    }
    raw.to_string()
}

/// Look up the signed-in user's display name + role for the nav bar.
/// Falls back to placeholders on a stray DB error - the nav must never
/// block on metadata.
pub async fn fetch_nav_user(state: &AppState, claims: &Claims) -> (String, String) {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT display_name, role FROM users WHERE id = ?")
            .bind(&claims.sub)
            .fetch_optional(&state.pool)
            .await
            .unwrap_or(None);
    row.unwrap_or_else(|| ("?".to_string(), claims.role.clone()))
}
