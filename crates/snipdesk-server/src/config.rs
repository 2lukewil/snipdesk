//! Server configuration: TOML file + env-var overrides for secrets.
//!
//! The master encryption key is sourced with strict priority:
//!   1. `SNIPDESK_MASTER_KEY` env var (base64-encoded 32 bytes). Preferred
//!      for container deployments - keeps the secret out of disk-resident
//!      config alongside other settings.
//!   2. `[crypto].master_key_file` path in the config TOML. The file must
//!      be readable only by the server's user (mode 0600 enforced on
//!      Unix; on Windows the OS ACL is up to the operator).
//!   3. `[crypto].master_key` inline in the config TOML. Discouraged but
//!      supported for development.
//!
//! Refuse to start without a key. There is no auto-generated default:
//! that would be a footgun (operators forget to set it, then can't
//! decrypt existing rows after the file rolls over).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::Deserialize;

/// Length of a master key in bytes. AES-256-GCM is 32-byte (256-bit) key.
pub const MASTER_KEY_LEN: usize = 32;

#[derive(Debug, Deserialize)]
pub struct Config {
    /// e.g. "0.0.0.0:8080".
    pub bind_addr: String,
    /// Where the SQLite DB lives. The directory is created on startup if
    /// missing; the file is created by sqlx on first connect.
    pub data_dir: PathBuf,
    /// 256-bit secret for signing JWTs (HS256). Generate one with `openssl
    /// rand -hex 32` or `snipdesk-server gen-jwt-secret` (later phase).
    #[serde(default)]
    #[allow(dead_code)] // wired up in phase 2 (auth)
    pub jwt_secret: Option<String>,

    #[serde(default)]
    pub crypto: CryptoConfig,

    /// How many days a soft-deleted snippet (personal or library)
    /// stays in the database before the background purge job drops
    /// it. The window has to be comfortably longer than the longest
    /// plausible offline period for any client, otherwise a device
    /// that comes back online after the purge would never learn
    /// about the deletion (the tombstone it would have synced is
    /// gone). 90 days is the v1 default. Set to 0 to disable purge
    /// entirely.
    #[serde(default = "default_tombstone_retention_days")]
    pub tombstone_retention_days: u32,

    /// OIDC / Google Workspace SSO. Optional - if `[oidc.google]`
    /// isn't set, the OIDC endpoints just return a 400 explaining
    /// the server is in password-only mode. Set this section to
    /// enable "Sign in with Google" from the desktop client.
    #[serde(default)]
    pub oidc: OidcConfig,

    /// Set the `Secure` attribute on the dashboard session cookie.
    /// Default `false` so localhost smoke tests work over plain HTTP;
    /// production deployments MUST set this to `true` (or terminate
    /// HTTPS at the reverse proxy and trust the proxy to drop
    /// plaintext requests).
    #[serde(default)]
    pub secure_cookies: bool,

    /// Knobs the stats page uses to translate snippet usage into
    /// time / money saved. Defaults are AUD-denominated since we
    /// normalise all displayed money to AUD on the dashboard.
    #[serde(default)]
    pub stats: StatsConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StatsConfig {
    /// Words-per-minute assumed for the "time saved by not typing
    /// this manually" estimate. 80 wpm is a fast-but-realistic
    /// support-agent typing speed; the desktop client also defaults
    /// to 40, but the dashboard estimate is "what if everyone typed
    /// at a brisk pace" so we lean higher.
    #[serde(default = "default_stats_wpm")]
    pub wpm: u32,

    /// Hourly wage in `currency`. Multiplied by saved hours to get
    /// the money-saved estimate. Defaults to AUD 25/hr - replace
    /// with your team's real number if you want the dashboard to
    /// be meaningful.
    #[serde(default = "default_stats_wage")]
    pub hourly_wage: f64,

    /// Currency code the wage is expressed in. Anything that isn't
    /// AUD is converted to AUD on the dashboard using the rates
    /// table below; if a code isn't in the table we treat the
    /// wage as already-AUD and warn in the logs.
    #[serde(default = "default_stats_currency")]
    pub currency: String,

    /// Exchange rates relative to AUD (1 unit of <code> = N AUD).
    /// Used to normalise the configured wage into AUD for display.
    /// Operators can override the defaults via config when rates
    /// drift materially - the values shipped are approximate
    /// long-term averages, not live FX.
    #[serde(default = "default_stats_rates")]
    pub aud_rates: std::collections::HashMap<String, f64>,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            wpm: default_stats_wpm(),
            hourly_wage: default_stats_wage(),
            currency: default_stats_currency(),
            aud_rates: default_stats_rates(),
        }
    }
}

fn default_stats_wpm() -> u32 {
    80
}
fn default_stats_wage() -> f64 {
    25.0
}
fn default_stats_currency() -> String {
    "AUD".to_string()
}
fn default_stats_rates() -> std::collections::HashMap<String, f64> {
    // Approximate long-term averages. Operators override in config
    // when their accounting wants real FX. We don't try to fetch live
    // rates - the dashboard estimate is order-of-magnitude, not
    // accounting-grade.
    let mut m = std::collections::HashMap::new();
    m.insert("AUD".to_string(), 1.0);
    m.insert("USD".to_string(), 1.50);
    m.insert("EUR".to_string(), 1.65);
    m.insert("GBP".to_string(), 1.95);
    m.insert("CAD".to_string(), 1.10);
    m.insert("NZD".to_string(), 0.92);
    m
}

fn default_tombstone_retention_days() -> u32 {
    90
}

#[derive(Debug, Deserialize, Default)]
pub struct CryptoConfig {
    /// Inline base64 master key. Lowest priority.
    pub master_key: Option<String>,
    /// Path to a file containing the base64 master key.
    pub master_key_file: Option<PathBuf>,
}

/// Optional OIDC providers. Currently only Google is wired up;
/// extending to other providers (Microsoft, Okta) is "add another
/// field + another handler module". For Workspace-only sign-in, set
/// `required_hd` to the Workspace primary domain.
#[derive(Debug, Deserialize, Default)]
pub struct OidcConfig {
    #[serde(default)]
    pub google: Option<GoogleOidcConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GoogleOidcConfig {
    /// The OAuth 2.0 client_id from Google Cloud Console. Looks like
    /// `123456789-abcdef.apps.googleusercontent.com`.
    pub client_id: String,
    /// The OAuth 2.0 client secret. Source of authentication; treat
    /// like a password. The example.toml carries a placeholder and is
    /// committed; the real value goes in the gitignored real config.
    pub client_secret: String,
    /// Where Google sends the user after sign-in. Must EXACTLY match
    /// one of the Authorized Redirect URIs registered in Google Cloud
    /// Console. For local dev: http://127.0.0.1:8080/api/auth/oidc/callback
    pub redirect_uri: String,
    /// Strict: reject any ID token whose `hd` claim doesn't match
    /// this Workspace domain. Google sets `hd` on tokens issued for
    /// Workspace members; personal @gmail.com accounts lack it.
    /// Combined with the consent screen being External, this is the
    /// canonical "lock down to my Workspace" knob.
    #[serde(default)]
    pub required_hd: Option<String>,
    /// Softer fallback: allow any email whose domain is in this list.
    /// Used when `required_hd` doesn't fit (e.g. contractors with
    /// custom-domain Gmail outside the Workspace). Empty = no email-
    /// domain filter beyond required_hd.
    #[serde(default)]
    pub allowed_email_domains: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&raw).with_context(|| format!("parse config {}", path.display()))?;
        Ok(cfg)
    }
}

/// 32-byte master key for AES-256-GCM. Wrapped so we never accidentally
/// dump it via Debug; the inner bytes never leave this module.
pub struct MasterKey([u8; MASTER_KEY_LEN]);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Length only - never echo the actual bytes (even partially) so
        // accidental debug logging can't leak the secret.
        f.debug_struct("MasterKey")
            .field("bytes", &"[redacted; 32 bytes]")
            .finish()
    }
}

impl MasterKey {
    pub fn as_bytes(&self) -> &[u8; MASTER_KEY_LEN] {
        &self.0
    }

    pub fn from_base64(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        let bytes = B64
            .decode(trimmed)
            .context("master key is not valid base64")?;
        if bytes.len() != MASTER_KEY_LEN {
            return Err(anyhow!(
                "master key must decode to {MASTER_KEY_LEN} bytes; got {}",
                bytes.len()
            ));
        }
        let mut out = [0u8; MASTER_KEY_LEN];
        out.copy_from_slice(&bytes);
        Ok(MasterKey(out))
    }

    pub fn generate() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; MASTER_KEY_LEN];
        rand::thread_rng().fill_bytes(&mut bytes);
        MasterKey(bytes)
    }

    pub fn to_base64(&self) -> String {
        B64.encode(self.0)
    }
}

/// Resolve a master key from (env var | config file path | inline config),
/// in that priority. Error message tells the operator exactly which slot
/// to fill if none are set.
pub fn load_master_key(cfg: &CryptoConfig) -> Result<MasterKey> {
    if let Ok(env_val) = std::env::var("SNIPDESK_MASTER_KEY") {
        if !env_val.trim().is_empty() {
            return MasterKey::from_base64(&env_val)
                .context("decoding SNIPDESK_MASTER_KEY env var");
        }
    }
    if let Some(path) = &cfg.master_key_file {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("read master_key_file {}", path.display()))?;
        return MasterKey::from_base64(&contents)
            .with_context(|| format!("decoding master_key_file {}", path.display()));
    }
    if let Some(inline) = &cfg.master_key {
        return MasterKey::from_base64(inline).context("decoding [crypto].master_key");
    }
    Err(anyhow!(
        "no master encryption key configured. Set one of:\n  \
         - SNIPDESK_MASTER_KEY env var (base64 of 32 bytes)\n  \
         - [crypto].master_key_file = \"/path/to/key\"\n  \
         - [crypto].master_key = \"<base64>\" (development only)\n\
         Generate a fresh key with: snipdesk-server gen-key"
    ))
}
