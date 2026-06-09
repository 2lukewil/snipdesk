//! Server-side update notification.
//!
//! Default-on: every `check_interval_hours`, polls the configured
//! release feed (GitHub Releases API by default) for the newest
//! release whose tag begins with `tag_prefix`. Strips the prefix to
//! get a version string, compares against `CARGO_PKG_VERSION`, and
//! flips an "is_newer" flag the dashboard reads to render a banner.
//!
//! Notification-only by design. The binary is never rewritten in
//! place:
//!
//!   - Container deployments shouldn't self-update at the binary
//!     layer; image immutability is the orchestrator's contract,
//!     and the operator's rollout pipeline (image pull + rolling
//!     restart) is the right control surface.
//!   - Bare-metal self-update needs published signed binaries to
//!     download. The release pipeline currently ships only Docker
//!     images, so there's nothing to swap to. When the pipeline
//!     grows binary artifacts + minisign signatures, a follow-up
//!     module can add the swap-and-exit step; until then,
//!     notification is the right level.
//!
//! Either way the user-visible signal ("a newer release is
//! available") shows up identically.

use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::RwLock;

use crate::config::UpdaterConfig;

/// Mirror of the latest poll. Cheap to clone (a few Strings + a
/// bool); the dashboard reads it on every page render.
#[derive(Debug, Default, Clone)]
pub struct UpdateStatus {
    /// Version string (tag with the configured prefix stripped),
    /// e.g. "0.2.0". None until the first successful poll.
    pub latest_version: Option<String>,
    /// Original tag name from the feed, e.g. "server-v0.2.0".
    pub latest_tag: Option<String>,
    /// HTML URL the dashboard banner links to. Points at the
    /// release page so an operator can read the notes in one click.
    pub html_url: Option<String>,
    /// Unix timestamp of the last successful poll. None on cold
    /// start; useful for debug + a future "last checked Xm ago"
    /// indicator.
    pub checked_at: Option<i64>,
    /// True when `latest_version` strictly exceeds the running
    /// binary's `CARGO_PKG_VERSION`. The dashboard banner only
    /// renders when this is true.
    pub is_newer: bool,
}

/// Shared cache wrapper. Empty until the first poll; the dashboard
/// reads via `current()` which clones the inner status under the
/// RwLock so renderers don't hold the lock for the page lifetime.
#[derive(Default)]
pub struct UpdateCache {
    pub status: RwLock<UpdateStatus>,
}

impl UpdateCache {
    pub async fn current(&self) -> UpdateStatus {
        self.status.read().await.clone()
    }
}

/// GitHub Releases API row. We only read four fields; #[serde] does
/// the rest of the response without complaint.
#[derive(Debug, Deserialize)]
struct ReleaseEntry {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// Spawn the periodic poller. Runs the first check immediately so a
/// fresh process learns about waiting releases without a startup
/// delay. Errors are logged at WARN and otherwise swallowed; a
/// transient network or rate-limit hiccup doesn't break the loop.
pub fn spawn_poller(cfg: UpdaterConfig, cache: Arc<UpdateCache>) {
    let interval = std::time::Duration::from_secs((cfg.check_interval_hours.max(1) as u64) * 3600);
    tokio::spawn(async move {
        loop {
            match fetch_once(&cfg).await {
                Ok(Some(entry)) => {
                    let latest = entry
                        .tag_name
                        .strip_prefix(&cfg.tag_prefix)
                        .unwrap_or(&entry.tag_name)
                        .to_string();
                    let current = env!("CARGO_PKG_VERSION");
                    let is_newer = semver_gt(&latest, current);
                    let mut status = cache.status.write().await;
                    let was_newer = status.is_newer;
                    status.latest_version = Some(latest.clone());
                    status.latest_tag = Some(entry.tag_name);
                    status.html_url = Some(entry.html_url);
                    status.checked_at = Some(chrono::Utc::now().timestamp());
                    status.is_newer = is_newer;
                    if is_newer && !was_newer {
                        tracing::info!(
                            current,
                            latest = %latest,
                            "updater: newer server release available"
                        );
                    }
                }
                Ok(None) => {
                    tracing::debug!("updater: no matching release tag found");
                }
                Err(e) => {
                    tracing::warn!("updater: check failed: {e}");
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

async fn fetch_once(cfg: &UpdaterConfig) -> anyhow::Result<Option<ReleaseEntry>> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("snipdesk-server/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let mut req = client.get(&cfg.release_feed_url);
    if let Some(token) = &cfg.github_token {
        req = req.bearer_auth(token);
    }
    let releases: Vec<ReleaseEntry> = req.send().await?.error_for_status()?.json().await?;
    // First non-draft / non-prerelease entry whose tag starts with
    // the configured prefix. GitHub returns the list newest-first,
    // so this is the highest matching release.
    Ok(releases
        .into_iter()
        .find(|r| !r.draft && !r.prerelease && r.tag_name.starts_with(&cfg.tag_prefix)))
}

/// Lightweight major.minor.patch comparator. We deliberately ignore
/// any `-prerelease` suffix on the patch component because the
/// candidate filter above already drops prerelease entries; if one
/// slips through, treating it as the bare numeric patch is the
/// safer side (won't trigger a false "update available").
fn semver_gt(a: &str, b: &str) -> bool {
    fn parse(s: &str) -> (u32, u32, u32) {
        let mut parts = s.split('.').take(3);
        let major = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let patch = parts
            .next()
            .unwrap_or("0")
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|x| x.parse().ok())
            .unwrap_or(0);
        (major, minor, patch)
    }
    parse(a) > parse(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_compares_numeric_triples() {
        assert!(semver_gt("0.2.0", "0.1.0"));
        assert!(semver_gt("1.0.0", "0.9.99"));
        assert!(semver_gt("0.1.10", "0.1.2"));
        assert!(!semver_gt("0.1.0", "0.1.0"));
        assert!(!semver_gt("0.1.0", "0.2.0"));
    }

    #[test]
    fn semver_handles_prerelease_suffixes_safely() {
        // Suffixes parse to patch=0; the candidate filter is what
        // actually keeps prereleases out, this just doesn't add a
        // false positive when a malformed tag sneaks through.
        assert!(!semver_gt("0.1.0-beta.1", "0.1.0"));
        assert!(semver_gt("0.2.0-rc.1", "0.1.0"));
    }
}
