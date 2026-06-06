//! Wire shapes for a pulled snippet library. Fetch impl lives in
//! `snipdesk-teams::shared_url`; the shapes live here so the DB layer can
//! `replace_team_snippets(...)` without pulling a network client into the
//! free build.
//!
//! Expected JSON:
//! ```json
//! {
//!   "version": 1,
//!   "snippets": [
//!     {
//!       "id": "opt-stable-id",
//!       "title": "Refund policy",
//!       "body": "…",
//!       "tags": ["billing", "policy"],
//!       "folder": "Billing/Refunds"
//!     }
//!   ]
//! }
//! ```
//! `id` is optional - if absent we derive from title so usage/variable
//! history survives refetches.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TeamSnippet {
    #[serde(default)]
    pub id: Option<String>,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// e.g. "Billing/Refunds". Empty = library root.
    #[serde(default)]
    pub folder: Option<String>,
}

/// `version` gates schema evolution; clients reject unknown values.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TeamLibrary {
    pub version: u32,
    pub snippets: Vec<TeamSnippet>,
}

/// Surfaced to the frontend for "last synced 5 min ago" and error badges.
#[derive(Debug, Clone, Serialize)]
pub struct SyncStatus {
    pub fetched_at_unix: Option<i64>,
    pub snippet_count: usize,
    pub last_error: Option<String>,
}

pub const FETCH_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Short so a hung DNS or firewall block at startup doesn't stall app launch.
pub const FETCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Saturating - clock skew into pre-1970 returns 0 instead of panicking.
pub fn system_time_to_unix(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
