//! Server configuration: TOML file + env-var overrides for secrets.
//!
//! The master encryption key is sourced with strict priority:
//!   1. `SNIPDESK_MASTER_KEY` env var (base64-encoded 32 bytes). Preferred
//!      for container deployments — keeps the secret out of disk-resident
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
}

#[derive(Debug, Deserialize, Default)]
pub struct CryptoConfig {
    /// Inline base64 master key. Lowest priority.
    pub master_key: Option<String>,
    /// Path to a file containing the base64 master key.
    pub master_key_file: Option<PathBuf>,
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
        // Length only — never echo the actual bytes (even partially) so
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
