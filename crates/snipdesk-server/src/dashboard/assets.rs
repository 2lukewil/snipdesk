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

/// Inline dashboard CSS. Small enough that a separate file would be
/// over-engineering; bumping it into a real stylesheet is a refactor
/// the day we add a third theme or designer feedback. For now the
/// goal is "looks like a serious tool, not a Bootstrap reskin."
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

pub async fn css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        DASHBOARD_CSS,
    )
}
