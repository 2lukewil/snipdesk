//! Axum router. Currently exposes only `/api/health` — every other
//! endpoint lands in this file as later phases add them (auth, snippet
//! sync, library, admin).

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use serde::Serialize;
use sqlx::SqlitePool;
use tower_http::trace::TraceLayer;

use crate::config::MasterKey;

/// Shared application state. Cloned per handler invocation; `pool` and
/// `master_key` are wrapped in Arc so the clones are cheap.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    /// Loaded at startup, used by the snippet-encryption layer in phase 3.
    #[allow(dead_code)]
    pub master_key: Arc<MasterKey>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .layer(TraceLayer::new_for_http())
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

/// Probe endpoint for load balancers / docker healthchecks. Confirms the
/// HTTP layer is up AND the DB is reachable. Returns 200 either way so
/// the response is parseable; flip to 503 if you want hard fail-stops.
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let db_ok = sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&state.pool)
        .await
        .map(|n| n == 1)
        .unwrap_or(false);
    (
        StatusCode::OK,
        Json(HealthResponse {
            status: "ok",
            version: env!("CARGO_PKG_VERSION"),
            db: db_ok,
        }),
    )
}
