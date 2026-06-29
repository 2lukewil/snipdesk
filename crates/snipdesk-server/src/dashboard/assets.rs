//! Static assets baked into the binary. `include_str!` pulls them in
//! at compile time so the deployed artifact is a single executable;
//! no external file fetches at runtime.

use axum::http::header;
use axum::response::IntoResponse;

/// Vendored htmx 1.9.12. Updating: download from
/// https://unpkg.com/htmx.org@<ver>/dist/htmx.min.js, replace the file.
/// We pin a version rather than tracking latest so a surprise htmx
/// release can't break the dashboard mid-deploy.
const HTMX_JS: &str = include_str!("static/htmx.min.js");

/// Vendored idiomorph 0.7.4 (the htmx-extension build, which bundles
/// Idiomorph and registers the `morph` extension). Lets list/sidebar
/// refreshes patch the DOM in place instead of replacing it, so scroll
/// position and the selected row survive the poll. Update the same way
/// as htmx: replace the file with a pinned release.
const IDIOMORPH_JS: &str = include_str!("static/idiomorph-ext.min.js");

/// Shared design tokens + scrollbar treatment, the single source of
/// truth the extension and desktop also consume. Pulled from the
/// repo-root `shared/styles/` tree at compile time, so the dashboard
/// stays one self-contained binary while the source lives in one place.
const TOKENS_CSS: &str = include_str!("../../../../shared/styles/tokens.css");
const SCROLLBARS_CSS: &str = include_str!("../../../../shared/styles/scrollbars.css");

/// Dashboard-specific CSS. Layout and components live here; the palette
/// now comes from the shared tokens above (linked first in the layout).
const DASHBOARD_CSS: &str = include_str!("static/dashboard.css");

pub async fn htmx() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        HTMX_JS,
    )
}

pub async fn idiomorph() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        IDIOMORPH_JS,
    )
}

pub async fn tokens_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        TOKENS_CSS,
    )
}

pub async fn scrollbars_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        SCROLLBARS_CSS,
    )
}

pub async fn css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        DASHBOARD_CSS,
    )
}
