//! Pull-only shared snippet library: GET a JSON doc from a user-supplied URL, merge into a read-only folder.
//! No auth - URL must be reachable unauthenticated (GitHub raw, S3, etc.).
//! Data shapes live in `snipdesk-core::shared_library` to keep HTTP deps off the DB layer.

use std::time::Duration;

use snipdesk_core::shared_library::{TeamLibrary, FETCH_CONNECT_TIMEOUT, FETCH_READ_TIMEOUT};

/// Blocking fetch - runs on the sync thread or from a "Sync now" command.
/// Timeouts are short: this runs at startup and a hung DNS lookup must not stall the app.
pub fn fetch(url: &str) -> Result<TeamLibrary, String> {
    if url.trim().is_empty() {
        return Err("shared library URL is empty".into());
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(FETCH_CONNECT_TIMEOUT)
        .timeout_read(FETCH_READ_TIMEOUT)
        .timeout_write(Duration::from_secs(30))
        .user_agent(concat!("SnipDesk/", env!("CARGO_PKG_VERSION")))
        .build();

    let resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("fetch failed: {e}"))?;

    let status = resp.status();
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }

    let body = resp
        .into_string()
        .map_err(|e| format!("read failed: {e}"))?;

    let lib: TeamLibrary =
        serde_json::from_str(&body).map_err(|e| format!("invalid shared library JSON: {e}"))?;

    if lib.version != 1 {
        return Err(format!(
            "unsupported shared library version {} (this client understands 1)",
            lib.version
        ));
    }

    Ok(lib)
}
