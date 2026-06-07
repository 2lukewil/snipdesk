//! Live FX rates for the admin dashboard's money/time saved card.
//!
//! Default-off: when `[fx]` isn't in the config, the dashboard uses
//! the static `[stats.aud_rates]` table that ships with the binary.
//! When configured, this module fetches USD-base rates from a free
//! no-auth provider on boot and refreshes every `cache_ttl_hours`,
//! overlaying the static table in-memory. Fetch failures don't
//! degrade the dashboard - we keep the previous live values (or
//! fall back to static), and the next tick retries.
//!
//! The estimate is order-of-magnitude. We don't need live rates to
//! tell a team they've saved "about 40 hours this quarter," so this
//! is an opt-in nice-to-have, not a critical path.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::RwLock;

use crate::config::FxConfig;

/// Shared FX state. Empty until the first successful fetch; the
/// dashboard reads it via `current_rates_or_static()` which merges
/// with the config's static `aud_rates` table for any codes the live
/// feed didn't return.
#[derive(Default)]
pub struct FxCache {
    /// Map of currency code -> "1 unit of code = N AUD". Same shape
    /// as `StatsConfig::aud_rates` so the dashboard can swap one for
    /// the other without other code changes.
    pub rates: RwLock<HashMap<String, f64>>,
}

/// open.er-api.com response shape. We treat any non-"success" result
/// as a soft failure - keep the previous cache, retry on the next
/// tick.
#[derive(Debug, Deserialize)]
struct ProviderResponse {
    #[serde(default)]
    result: String,
    #[serde(default)]
    base_code: String,
    #[serde(default)]
    rates: HashMap<String, f64>,
}

/// Boot-time + periodic refresh loop. Spawned from `main.rs`; runs
/// until the process exits. The first iteration runs immediately so
/// the first dashboard page render gets fresh numbers.
pub fn spawn_refresher(cfg: FxConfig, cache: Arc<FxCache>) {
    tokio::spawn(async move {
        let ttl = std::time::Duration::from_secs((cfg.cache_ttl_hours.max(1) as u64) * 3600);
        loop {
            match fetch_once(&cfg).await {
                Ok(map) => {
                    let n = map.len();
                    *cache.rates.write().await = map;
                    tracing::info!(rates = n, "fx: refreshed live rates");
                }
                Err(e) => {
                    tracing::warn!("fx: refresh failed: {e:#}");
                }
            }
            tokio::time::sleep(ttl).await;
        }
    });
}

/// Single fetch + conversion. Returns "1 unit of code = N AUD" for
/// every code the provider returned. Errors propagate so the caller
/// can log + retry without poisoning the cache.
async fn fetch_once(cfg: &FxConfig) -> anyhow::Result<HashMap<String, f64>> {
    let url = match cfg.provider.as_str() {
        // open.er-api.com is the default - free, no key, USD-base,
        // daily refresh on their side. Any path that returns the
        // same shape can be used by setting `provider_url`.
        "open.er-api.com" => "https://open.er-api.com/v6/latest/USD".to_string(),
        // Custom URL escape hatch for self-hosted/proxied FX feeds.
        // Must return the open.er-api.com response shape.
        _ if cfg.provider.starts_with("http") => cfg.provider.clone(),
        other => anyhow::bail!("unknown fx provider '{other}'"),
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(concat!("snipdesk-server/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp: ProviderResponse = client.get(&url).send().await?.json().await?;
    if resp.result != "success" {
        anyhow::bail!("provider returned non-success result");
    }
    if resp.rates.is_empty() {
        anyhow::bail!("provider returned empty rates");
    }
    // Provider gives us "1 unit of base_code = N units of X". We
    // want the inverse "1 unit of X = M units of AUD", so:
    //   rate_X_to_AUD = rates[AUD] / rates[X]
    // Skip codes we can't represent (zero/non-finite).
    let aud_rate = match resp.rates.get("AUD").copied() {
        Some(r) if r.is_finite() && r > 0.0 => r,
        _ => anyhow::bail!("provider response missing AUD rate"),
    };
    let mut out = HashMap::with_capacity(resp.rates.len());
    for (code, rate) in resp.rates {
        if !rate.is_finite() || rate <= 0.0 {
            continue;
        }
        // AUD itself = 1.0 by definition; the provider's own AUD
        // value matches that after the division.
        out.insert(code, aud_rate / rate);
    }
    tracing::debug!(
        base = %resp.base_code,
        n = out.len(),
        "fx: parsed live rates"
    );
    Ok(out)
}

/// Synchronous read with static fallback. Returns the live AUD
/// multiplier for `code` when available; otherwise falls through to
/// the config's static `aud_rates` table; finally falls back to 1.0
/// (with a warn-log) so the dashboard never panics on an unknown
/// code.
pub async fn rate_for(cache: &FxCache, static_table: &HashMap<String, f64>, code: &str) -> f64 {
    if let Some(r) = cache.rates.read().await.get(code).copied() {
        return r;
    }
    if let Some(r) = static_table.get(code).copied() {
        return r;
    }
    tracing::warn!("fx: no rate for '{code}' (live cache + static both miss)");
    1.0
}
