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

use anyhow::Result;

/// Top-level keyring "service" name. The OS groups credentials by this
/// label in its UI; the account suffix is the server URL.
const SERVICE: &str = "com.snipdesk.teams";

fn entry(server_url: &str) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(SERVICE, server_url)?)
}

pub fn store(server_url: &str, token: &str) -> Result<()> {
    entry(server_url)?.set_password(token)?;
    Ok(())
}

pub fn load(server_url: &str) -> Result<Option<String>> {
    match entry(server_url)?.get_password() {
        Ok(s) => Ok(Some(s)),
        // NoEntry is the normal "not signed in" case; map to None.
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete(server_url: &str) -> Result<()> {
    match entry(server_url)?.delete_credential() {
        Ok(()) => Ok(()),
        // Already absent — logout-on-not-signed-in is a no-op, not an error.
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
