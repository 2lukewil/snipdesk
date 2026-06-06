//! OS-keychain JWT storage. Scoped per server URL so signing into
//! different servers gives each its own slot; logging out clears just
//! the one. Windows Credential Manager / macOS Keychain / libsecret
//! handle the persistence.
//!
//! Why per-URL: the URL is the trust boundary. If a user has a personal
//! deployment at `https://a/` and a company one at `https://b/`, the
//! tokens shouldn't share a slot — a logout from one shouldn't drop the
//! other, and a stolen token from one shouldn't authenticate against
//! the other.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;

/// Top-level keyring "service" name. The OS groups credentials by this
/// label in its UI; the account suffix is the server URL.
const SERVICE: &str = "com.snipdesk.teams";

/// Process-lifetime cache of (server_url → token). Always read first;
/// only on a cache miss do we go to the OS keychain. This makes the
/// auth path resilient to two failure modes I've seen in the wild:
///   1. Some Windows configurations (AV products, corporate lockdowns)
///      let `set_password` *report* success but then the credential
///      isn't actually retrievable.
///   2. A background sync thread racing with the foreground UI: if
///      something briefly wipes the keychain entry, in-memory persists
///      for the current session and the user keeps working.
fn cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn entry(server_url: &str) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(SERVICE, server_url)?)
}

/// Always populates the in-memory cache; *attempts* the keychain write
/// for persistence across app restarts but doesn't fail the call if the
/// OS rejects it. The session stays usable either way.
pub fn store(server_url: &str, token: &str) -> Result<()> {
    if let Ok(mut c) = cache().lock() {
        c.insert(server_url.to_string(), token.to_string());
    }
    if let Err(e) = entry(server_url).and_then(|e| Ok(e.set_password(token)?)) {
        eprintln!("keychain store failed for {server_url}: {e} (session-only fallback)");
    }
    Ok(())
}

pub fn load(server_url: &str) -> Result<Option<String>> {
    // Cache first.
    if let Ok(c) = cache().lock() {
        if let Some(t) = c.get(server_url) {
            return Ok(Some(t.clone()));
        }
    }
    // Cache miss — try keychain (typical after a restart). Populate
    // the cache on hit so subsequent reads are O(1).
    match entry(server_url)?.get_password() {
        Ok(s) => {
            if let Ok(mut c) = cache().lock() {
                c.insert(server_url.to_string(), s.clone());
            }
            Ok(Some(s))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete(server_url: &str) -> Result<()> {
    if let Ok(mut c) = cache().lock() {
        c.remove(server_url);
    }
    match entry(server_url)?.delete_credential() {
        Ok(()) => Ok(()),
        // Already absent — logout-on-not-signed-in is a no-op, not an error.
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
