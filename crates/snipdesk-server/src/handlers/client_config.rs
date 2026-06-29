//! `GET /api/client-config` - deployment settings the clients need at
//! runtime, fetched after sign-in. Today this is just the support-ticket
//! link config (whether it's on, and the URL pattern the browser
//! extension scrapes a ticket reference with). Kept as its own endpoint
//! so new client-facing knobs can be added without touching `/api/me`.

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::auth::AuthUser;
use crate::http::AppState;

#[derive(Serialize)]
pub struct ClientConfig {
    pub ticket_link: TicketLinkConfig,
}

#[derive(Serialize)]
pub struct TicketLinkConfig {
    /// Whether the server stores ticket-referenced paste events. The
    /// extension only scrapes + reports when this is true.
    pub enabled: bool,
    /// Regex (JS syntax) whose first capture group is the ticket
    /// reference, applied to the active tab URL. `None` when unset, in
    /// which case the extension scrapes nothing even if enabled.
    pub url_pattern: Option<String>,
}

pub async fn client_config(State(state): State<AppState>, _auth: AuthUser) -> Json<ClientConfig> {
    Json(ClientConfig {
        ticket_link: TicketLinkConfig {
            enabled: state.ticket_link_enabled,
            url_pattern: state.ticket_url_pattern.clone(),
        },
    })
}
