//! Snippet-payload encryption at rest.
//!
//! Per the design doc, personal-snippet payloads (title/body/tags/folder)
//! are encrypted with the server's master key before insert and decrypted
//! on read. Database dumps are unreadable without the key.
//!
//! Construction:
//!   - AES-256-GCM via `aes_gcm::Aes256Gcm`.
//!   - Fresh 96-bit nonce per encryption (OsRng).
//!   - Associated data binds the ciphertext to `(snippet_id, owner_id,
//!     version)` so a server-side swap of ciphertext between rows or
//!     between users would fail authentication on decrypt.
//!
//! The payload itself is the JSON encoding of `SnippetPayload`. Encoding
//! as JSON means schema additions are append-only — older rows with
//! fewer fields decrypt into a struct that fills the missing fields with
//! `#[serde(default)]`, no migration of column structure required.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::MasterKey;

/// The plaintext shape of every personal snippet, exactly as the client
/// sends/receives it. New optional fields can be added later with
/// `#[serde(default)]` — older rows will decrypt with the missing fields
/// filled in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnippetPayload {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub folder_path: Option<String>,
}

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("encryption failed")]
    Encrypt,
    #[error("decryption failed (tampered ciphertext or wrong context)")]
    Decrypt,
    #[error("payload not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// What the storage layer holds for one snippet.
#[derive(Debug, Clone)]
pub struct EncryptedBlob {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
}

/// Bind the ciphertext to its (snippet_id, owner_id, version) tuple.
/// A row-level swap (changing which user the ciphertext is filed under,
/// or which row id holds it, or which version it claims to be) fails
/// authentication on decrypt — the attacker would have to forge a
/// GCM tag, which they can't without the key.
fn associated_data(snippet_id: &str, owner_id: &str, version: i64) -> Vec<u8> {
    format!("{snippet_id}|{owner_id}|{version}").into_bytes()
}

pub fn encrypt_payload(
    key: &MasterKey,
    payload: &SnippetPayload,
    snippet_id: &str,
    owner_id: &str,
    version: i64,
) -> Result<EncryptedBlob, CryptoError> {
    let plaintext = serde_json::to_vec(payload)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key.as_bytes()));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let aad = associated_data(snippet_id, owner_id, version);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::Encrypt)?;
    Ok(EncryptedBlob {
        ciphertext,
        nonce: nonce.to_vec(),
    })
}

pub fn decrypt_payload(
    key: &MasterKey,
    blob: &EncryptedBlob,
    snippet_id: &str,
    owner_id: &str,
    version: i64,
) -> Result<SnippetPayload, CryptoError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key.as_bytes()));
    let nonce = Nonce::from_slice(&blob.nonce);
    let aad = associated_data(snippet_id, owner_id, version);
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &blob.ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::Decrypt)?;
    let payload: SnippetPayload = serde_json::from_slice(&plaintext)?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SnippetPayload {
        SnippetPayload {
            title: "Refund follow-up".into(),
            body: "Hi {customer_name}, your refund…".into(),
            tags: vec!["billing".into(), "refund".into()],
            folder_path: Some("Billing/Refunds".into()),
        }
    }

    // Round-trip: encrypt then decrypt with matching context returns the
    // original payload byte-for-byte. The whole point of the layer.
    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = MasterKey::generate();
        let original = sample();
        let blob = encrypt_payload(&key, &original, "snip-1", "owner-1", 1).unwrap();
        let recovered = decrypt_payload(&key, &blob, "snip-1", "owner-1", 1).unwrap();
        assert_eq!(original, recovered);
    }

    // Two encrypts of the SAME plaintext must produce different
    // ciphertext (fresh nonce each time). If this regresses, our crypto
    // has become deterministic, which leaks "same plaintext under same
    // key" — a real-world AES-GCM bug pattern when someone hardcodes a
    // nonce.
    #[test]
    fn fresh_nonce_per_encryption() {
        let key = MasterKey::generate();
        let p = sample();
        let a = encrypt_payload(&key, &p, "id", "owner", 1).unwrap();
        let b = encrypt_payload(&key, &p, "id", "owner", 1).unwrap();
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    // Each mis-matched context field must independently break decrypt.
    // Catches a regression where AD construction silently dropped a
    // component (e.g., used owner only) — that would allow row swaps.
    #[test]
    fn mismatched_context_fails_decrypt() {
        let key = MasterKey::generate();
        let blob = encrypt_payload(&key, &sample(), "snip-A", "owner-1", 1).unwrap();
        // wrong snippet id
        assert!(decrypt_payload(&key, &blob, "snip-B", "owner-1", 1).is_err());
        // wrong owner
        assert!(decrypt_payload(&key, &blob, "snip-A", "owner-2", 1).is_err());
        // wrong version
        assert!(decrypt_payload(&key, &blob, "snip-A", "owner-1", 2).is_err());
    }

    // Wrong key fails decrypt. If this passed, the encryption would be
    // effectively a no-op against an attacker who can guess the key
    // length — but it's GCM, so any single-bit key flip should fail
    // authentication.
    #[test]
    fn wrong_key_fails_decrypt() {
        let key_a = MasterKey::generate();
        let key_b = MasterKey::generate();
        let blob = encrypt_payload(&key_a, &sample(), "id", "owner", 1).unwrap();
        assert!(decrypt_payload(&key_b, &blob, "id", "owner", 1).is_err());
    }
}
