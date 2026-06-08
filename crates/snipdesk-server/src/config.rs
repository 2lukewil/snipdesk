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

    /// Origins allowed to make cross-origin JSON-API requests. Empty
    /// (default) means no CORS layer at all - same-origin only, which
    /// matches the v1 desktop-client + dashboard topology where both
    /// hit the server on its own host. Populate this when a separate
    /// web frontend lands and needs to talk to `/api/*`. Each origin
    /// must include scheme and (if non-default) port, e.g.
    /// `["https://app.example.com", "http://localhost:5173"]`.
    /// Credentials are always allowed on the listed origins (the
    /// JSON API uses `Authorization: Bearer ...`, not cookies, but
    /// it's the sane default for the dashboard cookie case too).
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,

    /// Knobs the stats page uses to translate snippet usage into
    /// time / money saved. Defaults are AUD-denominated since we
    /// normalise all displayed money to AUD on the dashboard.
    #[serde(default)]
    pub stats: StatsConfig,

    /// Optional live FX feed for the money-saved estimate. When
    /// present, the server fetches the provider on boot and
    /// `cache_ttl_hours` later, overlaying the static
    /// `[stats.aud_rates]` table with fresh numbers. When absent
    /// (default), only the static table is used - no outbound HTTP
    /// from the server.
    #[serde(default)]
    pub fx: Option<FxConfig>,

    /// Dashboard brand strings. Lets a redeployment label its
    /// admin dashboard with the operator's own product name
    /// instead of "SnipDesk". Server-side branding is intentionally
    /// minimal - just the visible labels in the layout / login /
    /// member-blocked templates - so the binary stays reusable
    /// across deployments.
    #[serde(default)]
    pub brand: BrandConfig,

    /// Periodic check for newer server releases. Notification-only:
    /// when a newer release is detected, a banner surfaces in the
    /// dashboard and an info log fires; the binary is never
    /// rewritten in-place. Container deployments roll forward via
    /// image-tag updates; bare-metal operators swap the binary
    /// during their maintenance window.
    #[serde(default)]
    pub updater: UpdaterConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct UpdaterConfig {
    /// When false, the background poller never starts and the
    /// dashboard banner stays hidden regardless of GitHub state.
    /// Default on so a freshly-deployed server picks up release
    /// announcements without any extra configuration.
    #[serde(default = "default_updater_enabled")]
    pub enabled: bool,
    /// How often to re-check the release feed. Default 6h is
    /// sub-quota for the unauthenticated GitHub API (60 req/hr per
    /// IP) even with several instances behind one NAT, and
    /// catches a release within half a day.
    #[serde(default = "default_updater_interval")]
    pub check_interval_hours: u32,
    /// Release feed URL. Defaults to the public releases endpoint
    /// for the upstream repo; an air-gapped mirror or a fork can
    /// point this at its own endpoint serving the same JSON shape.
    #[serde(default = "default_release_feed_url")]
    pub release_feed_url: String,
    /// Tag prefix used to filter the feed for server-only
    /// releases (the desktop client and the server share one repo
    /// but have separate tag streams). Default matches
    /// release-server.yml's trigger.
    #[serde(default = "default_updater_tag_prefix")]
    pub tag_prefix: String,
    /// Optional GitHub token to lift the rate-limit ceiling from
    /// 60/hr (unauthenticated) to 5000/hr. Only needed if many
    /// snipdesk-server instances share an outbound IP. A token
    /// with no scopes is sufficient for public-repo reads.
    #[serde(default)]
    pub github_token: Option<String>,
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            enabled: default_updater_enabled(),
            check_interval_hours: default_updater_interval(),
            release_feed_url: default_release_feed_url(),
            tag_prefix: default_updater_tag_prefix(),
            github_token: None,
        }
    }
}

fn default_updater_enabled() -> bool {
    true
}
fn default_updater_interval() -> u32 {
    6
}
fn default_release_feed_url() -> String {
    "https://api.github.com/repos/2lukewil/snipdesk/releases?per_page=20".to_string()
}
fn default_updater_tag_prefix() -> String {
    "server-v".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct BrandConfig {
    /// Shown in the browser title, nav header, and login card.
    /// Defaults to "SnipDesk" so an existing deployment that
    /// doesn't ship a `[brand]` block continues to display the
    /// stock name.
    #[serde(default = "default_brand_name")]
    pub name: String,
}

impl Default for BrandConfig {
    fn default() -> Self {
        Self {
            name: default_brand_name(),
        }
    }
}

fn default_brand_name() -> String {
    "SnipDesk".to_string()
}

/// Live FX feed configuration. Optional; absence keeps the server
/// fully offline-capable. We don't ship a provider API key in the
/// default - the supported providers are key-free.
#[derive(Debug, Deserialize, Clone)]
pub struct FxConfig {
    /// Provider identifier. `"open.er-api.com"` is the supported
    /// default (free, no key, USD-base). Any value starting with
    /// `http` is treated as a custom URL returning the same response
    /// shape - useful for self-hosted proxies and tests.
    #[serde(default = "default_fx_provider")]
    pub provider: String,
    /// How long to cache the fetched rates before fetching again.
    /// Minimum 1, default 24 hours; FX moves slowly enough that any
    /// shorter cadence wastes provider quota.
    #[serde(default = "default_fx_ttl_hours")]
    pub cache_ttl_hours: u32,
}

impl Default for FxConfig {
    fn default() -> Self {
        Self {
            provider: default_fx_provider(),
            cache_ttl_hours: default_fx_ttl_hours(),
        }
    }
}

fn default_fx_provider() -> String {
    "open.er-api.com".to_string()
}
fn default_fx_ttl_hours() -> u32 {
    24
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
    // Approximate long-term averages, 1 unit of <code> = N AUD.
    // Operators override in config when their accounting wants real
    // FX, and (when enabled) the FX poller can refresh these from
    // a live source. The static table is the cold-start default and
    // the offline fallback.
    let mut m = std::collections::HashMap::new();
    m.insert("AUD".to_string(), 1.00);
    m.insert("USD".to_string(), 1.50);
    m.insert("EUR".to_string(), 1.65);
    m.insert("GBP".to_string(), 1.95);
    m.insert("CAD".to_string(), 1.10);
    m.insert("NZD".to_string(), 0.92);
    m.insert("JPY".to_string(), 0.0098);
    m.insert("CHF".to_string(), 1.70);
    m.insert("INR".to_string(), 0.018);
    m.insert("SGD".to_string(), 1.12);
    m.insert("HKD".to_string(), 0.19);
    m.insert("ZAR".to_string(), 0.082);
    m.insert("BRL".to_string(), 0.27);
    m.insert("MXN".to_string(), 0.080);
    m.insert("KRW".to_string(), 0.0011);
    m.insert("SEK".to_string(), 0.14);
    m.insert("NOK".to_string(), 0.14);
    m.insert("DKK".to_string(), 0.22);
    m.insert("PLN".to_string(), 0.38);
    m.insert("CZK".to_string(), 0.066);
    m.insert("TRY".to_string(), 0.045);
    m.insert("AED".to_string(), 0.41);
    m.insert("CNY".to_string(), 0.21);
    m.insert("THB".to_string(), 0.041);
    m.insert("IDR".to_string(), 0.000093);
    m.insert("PHP".to_string(), 0.027);
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
