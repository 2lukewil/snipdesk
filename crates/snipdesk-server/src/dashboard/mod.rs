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

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post, put};
use axum::Router;

use crate::http::{AppState, BODY_LIMIT_LARGE};

/// Mount every dashboard route onto the shared Axum router. Called from
/// `http::router` so the dashboard and `/api/*` share the same listener,
/// state, and TLS surface.
pub fn routes() -> Router<AppState> {
    Router::new()
        // Public-facing
        .route("/", get(pages::index))
        .route("/dashboard/login", post(pages::login_submit))
        .route("/dashboard/logout", post(pages::logout))
        // First-run setup: only effective while the users table is
        // empty (the INSERT no-ops otherwise), so leaving the route
        // mounted permanently is harmless.
        .route("/dashboard/setup", post(pages::setup_submit))
        // SSO entry for the dashboard. The IdP callback URL stays
        // on the API surface (/api/auth/oidc/:provider/callback) so
        // operators only register one redirect URI per provider;
        // this handler just stashes a Dashboard FlowOrigin and 302s
        // off to the IdP. The shared callback then dispatches on the
        // origin and finishes by setting the session cookie.
        .route(
            "/dashboard/oidc/:provider/start",
            get(pages::dashboard_oidc_start),
        )
        // Admin-only (extractor enforces; non-admins get bounced)
        .route(
            "/dashboard/users",
            get(pages::users_page).post(pages::user_create_row),
        )
        // Fragment endpoints for the 5-second polling tick on the
        // users / library pages. Return inner HTML only; the parent
        // page provides the container.
        .route("/dashboard/users/rows", get(pages::users_rows))
        .route("/dashboard/library/cards", get(pages::library_cards))
        // Blank editor-pane create form for the list pane's "+" button.
        .route("/dashboard/library/new", get(pages::library_new_editor))
        // Folder-tree fragment for the library sidebar polling sweep.
        .route("/dashboard/library/folders", get(pages::library_folders))
        .route("/dashboard/users/:id", put(pages::user_update_row))
        .route("/dashboard/users/:id", delete(pages::user_delete_row))
        .route("/dashboard/library", get(pages::library_page))
        // Library download. GET serves direct-URL use (search +
        // folder query params); POST serves the selection modal's
        // explicit id list (large cap: the id list scales with the
        // library).
        .route(
            "/dashboard/library/export",
            get(pages::library_export)
                .post(pages::library_export_selected)
                .layer(DefaultBodyLimit::max(BODY_LIMIT_LARGE)),
        )
        // Export half of the selection modal: the library as a tree
        // fragment with everything selected.
        .route(
            "/dashboard/library/export/picker",
            get(pages::library_export_picker),
        )
        // Import: tree preview fragment + confirm. Both posts carry
        // the whole file content / entry list as form fields, so
        // they need the large body cap.
        .route(
            "/dashboard/library/import",
            post(pages::library_import_confirm).layer(DefaultBodyLimit::max(BODY_LIMIT_LARGE)),
        )
        .route(
            "/dashboard/library/import/preview",
            post(pages::library_import_preview).layer(DefaultBodyLimit::max(BODY_LIMIT_LARGE)),
        )
        .route(
            "/dashboard/library",
            post(pages::library_create).layer(DefaultBodyLimit::max(BODY_LIMIT_LARGE)),
        )
        .route(
            "/dashboard/library/:id",
            put(pages::library_update).layer(DefaultBodyLimit::max(BODY_LIMIT_LARGE)),
        )
        .route("/dashboard/library/:id", delete(pages::library_delete))
        // Inline edit: GET returns the edit form fragment, /card
        // returns the read-only card (for cancel), /move is the
        // drag-drop endpoint.
        .route("/dashboard/library/:id/edit", get(pages::library_edit_form))
        .route(
            "/dashboard/library/:id/card",
            get(pages::library_card_fragment),
        )
        .route("/dashboard/library/:id/move", put(pages::library_move))
        // Folder rename / nest / unnest. POST not PUT because it
        // operates on a folder name (form data) rather than a
        // single snippet id in the path. Validates internally and
        // mass-bumps every affected snippet's version.
        .route(
            "/dashboard/library/folders/move",
            post(pages::library_folder_move),
        )
        // "+ New folder" button. Creates an empty folder row so it
        // appears in the sidebar even before any snippet lands in it.
        .route(
            "/dashboard/library/folders/create",
            post(pages::library_folder_create),
        )
        // Manual reorder. Receives a complete ordered list of
        // sibling paths under one parent and rewrites their
        // sort_order to match. Idempotent.
        .route(
            "/dashboard/library/folders/reorder",
            post(pages::library_folder_reorder),
        )
        // Folder delete: GET serves the contents-aware confirm modal
        // fragment, POST performs it (mode=move sends contents to
        // Unfiled, mode=delete tombstones them too).
        .route(
            "/dashboard/library/folders/delete/confirm",
            get(pages::library_folder_delete_confirm),
        )
        .route(
            "/dashboard/library/folders/delete",
            post(pages::library_folder_delete),
        )
        // Per-user detail + stats. Detail uses the same /users/:id
        // path as the JSON PUT/DELETE because GET there is unused;
        // axum routes on (path, method) so the methods coexist.
        .route("/dashboard/users/:id", get(pages::user_detail_page))
        .route("/dashboard/stats", get(pages::stats_page))
        .route(
            "/dashboard/library/insights",
            get(pages::library_insights_page),
        )
        .route(
            "/dashboard/library/snippet-tickets/:id",
            get(pages::library_snippet_tickets_page),
        )
        .route("/dashboard/audit", get(pages::audit_page))
        // Static assets - vendored htmx + a small CSS file. Served as
        // raw bytes via plain handlers (no fs reads at runtime; the
        // bytes are baked into the binary by include_str!).
        .route("/static/htmx.min.js", get(assets::htmx))
        .route("/static/idiomorph-ext.min.js", get(assets::idiomorph))
        .route("/static/dashboard.css", get(assets::css))
}
