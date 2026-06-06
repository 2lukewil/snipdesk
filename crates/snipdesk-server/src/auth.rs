//! Password hashing, JWT issuance/verification, and the `RequireUser`
//! extractor used by protected handlers.
//!
//! Crypto choices:
//!   - **Argon2id** for password storage (OWASP's current recommendation).
//!     Default parameters from the `argon2` crate - they map roughly to
//!     "expensive enough that a single guess takes ~50ms on a modern CPU,"
//!     which is the right cost for an internal tool.
//!   - **HS256** JWTs (symmetric HMAC). The server holds the secret;
//!     clients don't need to verify, just present. 24h TTL.

use argon2::password_hash::{rand_core::OsRng, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Argon2, PasswordHash};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::http::AppState;

/// 24-hour session lifetime - matches docs/server-design.md.
pub const SESSION_TTL_HOURS: i64 = 24;

/// Pre-computed sentinel hash used in `verify_password_constant_time`.
/// When a login attempt names an unknown user we still run a verify
/// against this - same wall-clock cost as a real verify - so an attacker
/// can't enumerate registered emails by measuring response time.
static SENTINEL_HASH: Lazy<String> = Lazy::new(|| {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(b"sentinel-never-matches", &salt)
        .expect("argon2 sentinel hash")
        .to_string()
});

pub fn hash_password(plaintext: &str) -> Result<String, ApiError> {
    let salt = SaltString::generate(&mut OsRng);
    let hashed = Argon2::default()
        .hash_password(plaintext.as_bytes(), &salt)?
        .to_string();
    Ok(hashed)
}

/// Verify `plaintext` against an Argon2id hash. If `stored_hash` is
/// `None` (user doesn't exist), still does a verify against the sentinel
/// so timing matches.
pub fn verify_password_constant_time(plaintext: &str, stored_hash: Option<&str>) -> bool {
    let hash_str = stored_hash.unwrap_or(&SENTINEL_HASH);
    let Ok(parsed) = PasswordHash::new(hash_str) else {
        return false;
    };
    Argon2::default()
        .verify_password(plaintext.as_bytes(), &parsed)
        .is_ok()
        && stored_hash.is_some()
}

/// JWT payload. `sub` is the user id; we include `role` so handlers can
/// gate admin-only endpoints without an extra DB roundtrip per request.
/// `Clone` so the dashboard layer can hand the same claims to both its
/// admin extractor and an inline `AuthUser` adapter when delegating to
/// the underlying JSON handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub role: String,
    pub exp: usize,
    pub iat: usize,
}

pub fn issue_token(user_id: &str, role: &str, secret: &str) -> Result<String, ApiError> {
    if secret.is_empty() {
        return Err(ApiError::internal(
            "JWT secret not configured (set jwt_secret in config)",
        ));
    }
    let now = Utc::now();
    let exp = now + Duration::hours(SESSION_TTL_HOURS);
    let claims = Claims {
        sub: user_id.to_string(),
        role: role.to_string(),
        iat: now.timestamp() as usize,
        exp: exp.timestamp() as usize,
    };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?;
    Ok(token)
}

pub fn verify_token(token: &str, secret: &str) -> Result<Claims, ApiError> {
    if secret.is_empty() {
        return Err(ApiError::internal("JWT secret not configured"));
    }
    let validation = Validation::default();
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map_err(|_| ApiError::unauthorized("invalid_token", "invalid or expired session"))?;
    Ok(data.claims)
}

/// Generate a base64-encoded 256-bit JWT secret. Surfaced as the
/// `gen-jwt-secret` CLI subcommand.
pub fn generate_jwt_secret() -> String {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    B64.encode(bytes)
}

/// Extractor: pulls `Authorization: Bearer <jwt>` off the request and
/// returns the verified claims. Handlers that need an authenticated user
/// declare `auth: AuthUser` as a parameter; if no/invalid token, the
/// request short-circuits with 401 before the handler runs.
pub struct AuthUser(pub Claims);

impl AuthUser {
    /// Returns `Ok(())` when the caller has the `admin` role; otherwise a
    /// 403-grade ApiError. Used by library / admin handlers that any
    /// signed-in member can READ but only admins can WRITE.
    pub fn require_admin(&self) -> Result<(), ApiError> {
        if self.0.role == "admin" {
            Ok(())
        } else {
            Err(ApiError::forbidden(
                "admin_required",
                "this action requires admin privileges",
            ))
        }
    }
}

#[axum::async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| {
                ApiError::unauthorized("missing_auth", "missing Authorization header")
            })?;
        let token = header
            .strip_prefix("Bearer ")
            .ok_or_else(|| ApiError::unauthorized("bad_auth_scheme", "expected Bearer token"))?;
        let claims = verify_token(token.trim(), &state.jwt_secret)?;

        // Check is_disabled at every request. An admin disabling the
        // account from the dashboard should take effect immediately -
        // not at JWT expiry 24h later. We piggy-back on the same row
        // read that updates last_seen_at, so this is one round-trip,
        // not two. If the row vanished entirely (admin deleted the
        // user), we treat it like a disabled account: terminate the
        // session right now.
        let now = chrono::Utc::now().timestamp();
        let status: Option<(i64,)> =
            sqlx::query_as("UPDATE users SET last_seen_at = ? WHERE id = ? RETURNING is_disabled")
                .bind(now)
                .bind(&claims.sub)
                .fetch_optional(&state.pool)
                .await
                .unwrap_or(None);

        match status {
            None => {
                // Account no longer exists (deleted between issue and
                // now). Distinct error code so the desktop client can
                // wipe its session rather than just retrying.
                return Err(ApiError::forbidden(
                    "account_gone",
                    "your account no longer exists; sign in again",
                ));
            }
            Some((disabled,)) if disabled != 0 => {
                return Err(ApiError::forbidden(
                    "account_disabled",
                    "your account is disabled - contact your administrator",
                ));
            }
            _ => {}
        }

        Ok(AuthUser(claims))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Round-trip ensures we can issue a token and verify it. If this
    // ever fails it's almost certainly a header/secret mismatch in CI.
    #[test]
    fn jwt_round_trip() {
        let secret = "test-secret-not-for-production";
        let token = issue_token("u1", "member", secret).unwrap();
        let claims = verify_token(&token, secret).unwrap();
        assert_eq!(claims.sub, "u1");
        assert_eq!(claims.role, "member");
    }

    // Wrong secret must reject. Catches accidentally allowing forged
    // tokens through a misconfigured validator.
    #[test]
    fn jwt_rejects_wrong_secret() {
        let token = issue_token("u1", "member", "secret-A").unwrap();
        assert!(verify_token(&token, "secret-B").is_err());
    }

    // Right password verifies; wrong rejects; no-such-user rejects.
    // Sentinel-hash path must not crash - if it did, the
    // timing-equalization would itself become a side channel.
    #[test]
    fn password_verify_paths() {
        let hash = hash_password("hunter2").unwrap();
        assert!(verify_password_constant_time("hunter2", Some(&hash)));
        assert!(!verify_password_constant_time("wrong", Some(&hash)));
        assert!(!verify_password_constant_time("anything", None));
    }
}
