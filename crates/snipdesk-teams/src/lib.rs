//! Paid-tier features. Split from core so the free build's dep tree
//! contains no HTTP / auth code.
//!
//! Layout:
//!   - `api`         — typed HTTP client for the snipdesk-server backend
//!   - `credentials` — keychain-backed JWT storage scoped per server URL
//!   - `shared_url`  — legacy pull-only JSON team library (retired in phase 5)
//!   - `sync`        — two-way sync engine: pushes local dirty rows,
//!     pulls remote changes since high-water-mark

pub mod api;
pub mod credentials;
pub mod shared_url;
pub mod sync;
