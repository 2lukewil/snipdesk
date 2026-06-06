//! Server-rendered htmx dashboard.
//!
//! This is the admin UI for managing users and the shared snippet
//! library. Internal tooling: it's mounted on the same Axum router as
//! the JSON API, behind the same TLS, and gated to admins by a
//! cookie-based session.
//!
//! Design notes:
//!   - **Cookie auth** because browsers don't naturally attach
//!     `Authorization: Bearer ...` to form posts. The JWT we already
//!     issue for the desktop client is re-used here, just delivered via
//!     an HttpOnly cookie. Sign-out clears the cookie.
//!   - **No template engine**. Four templates × `include_str!` +
//!     `{{KEY}}` substitution is plenty. The day this exceeds ~ten
//!     pages, swap to askama; right now a proc-macro would buy nothing.
//!   - **htmx is vendored** under `static/htmx.min.js`. Single-binary
//!     deployment is one of the design-doc goals; depending on a CDN
//!     would silently break air-gapped installs.
//!   - **All routes require admin.** Non-admin members can log in to
//!     `/` but get bounced to a "members can't access the dashboard"
//!     error page. They're meant to use the desktop client.

pub mod assets;
pub mod pages;
pub mod session;

use axum::routing::{delete, get, post, put};
use axum::Router;

use crate::http::AppState;

/// Mount every dashboard route onto the shared Axum router. Called from
/// `http::router` so the dashboard and `/api/*` share the same listener,
/// state, and TLS surface.
pub fn routes() -> Router<AppState> {
    Router::new()
        // Public-facing
        .route("/", get(pages::index))
        .route("/dashboard/login", post(pages::login_submit))
        .route("/dashboard/logout", post(pages::logout))
        // Admin-only (extractor enforces; non-admins get bounced)
        .route(
            "/dashboard/users",
            get(pages::users_page).post(pages::user_create_row),
        )
        .route("/dashboard/users/:id", put(pages::user_update_row))
        .route("/dashboard/users/:id", delete(pages::user_delete_row))
        .route("/dashboard/library", get(pages::library_page))
        .route("/dashboard/library", post(pages::library_create))
        .route("/dashboard/library/:id", put(pages::library_update))
        .route("/dashboard/library/:id", delete(pages::library_delete))
        // Static assets — vendored htmx + a small CSS file. Served as
        // raw bytes via plain handlers (no fs reads at runtime; the
        // bytes are baked into the binary by include_str!).
        .route("/static/htmx.min.js", get(assets::htmx))
        .route("/static/dashboard.css", get(assets::css))
}
