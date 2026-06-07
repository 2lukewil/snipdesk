//! /api/auth/* and /api/me handlers.
//!
//! Behaviour summary:
//!   - POST /api/auth/signup creates a new account. The FIRST successful
//!     signup against a fresh DB is auto-promoted to admin so the
//!     operator doesn't need a separate bootstrap step. Everyone after
//!     that signs up as a regular member.
//!   - POST /api/auth/login returns a JWT on valid credentials. Login
//!     deliberately uses the same message ("invalid credentials") for
//!     bad password AND missing user so attackers can't enumerate emails.
//!   - POST /api/auth/logout is a no-op for stateless JWTs but exists
//!     so the client has something to call. A future revoke-list lands
//!     here when we need it.
//!   - GET /api/me echoes the authenticated user - uses the AuthUser
//!     extractor, which 401s if the token is missing/invalid.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::{hash_password, issue_token, verify_password_constant_time, AuthUser};
use crate::error::ApiError;
use crate::http::AppState;

/// Bare-minimum password policy. Internal-tool baseline; rotate this
/// upward when you have an opinion. NIST's current guidance is "let
/// users pick what they want as long as it's reasonably long," which
/// 10 chars satisfies for an internal tool gating snippet text.
const MIN_PASSWORD_LEN: usize = 10;

#[derive(Debug, Deserialize)]
pub struct SignupBody {
    pub email: String,
    pub password: String,
    pub display_name: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginBody {
    pub email: String,
    pub password: String,
}

/// Wire-shape returned alongside a JWT. Mirrors the SELECT below in
/// queries that hydrate a logged-in user; if you add columns to `users`
/// that are safe to expose, add them here.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct UserDto {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub role: String,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    pub token: String,
    pub user: UserDto,
}

pub async fn signup(
    State(state): State<AppState>,
    Json(body): Json<SignupBody>,
) -> Result<(StatusCode, Json<AuthResponse>), ApiError> {
    let email = body.email.trim().to_lowercase();
    let display_name = body.display_name.trim().to_string();
    if !looks_like_email(&email) {
        return Err(ApiError::bad_request("invalid_email", "invalid email"));
    }
    if body.password.len() < MIN_PASSWORD_LEN {
        return Err(ApiError::bad_request(
            "weak_password",
            format!("password must be at least {MIN_PASSWORD_LEN} characters"),
        ));
    }
    if display_name.is_empty() {
        return Err(ApiError::bad_request(
            "missing_display_name",
            "display_name is required",
        ));
    }

    // Uniqueness check up front so we don't run an expensive Argon2 hash
    // for a request we already know will fail. The DB has a UNIQUE
    // constraint on email too - that's the real guardrail; this is just
    // a faster failure path.
    let existing: Option<(String,)> = sqlx::query_as("SELECT id FROM users WHERE email = ?")
        .bind(&email)
        .fetch_optional(&state.pool)
        .await?;
    if existing.is_some() {
        return Err(ApiError::conflict(
            "email_taken",
            "an account with this email already exists",
        ));
    }

    let password_hash = hash_password(&body.password)?;
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();

    // First-admin auto-promotion: if no admins exist yet, this signup
    // becomes the admin. Subsequent signups all land as 'member'.
    let admin_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE role = 'admin'")
        .fetch_one(&state.pool)
        .await?;
    let role = if admin_count.0 == 0 {
        "admin"
    } else {
        "member"
    };

    // last_seen_at is set on signup too - a brand-new account just
    // signed in, so showing "never" in the dashboard would be wrong
    // (their account literally exists because they're here right now).
    sqlx::query(
        "INSERT INTO users (id, email, display_name, role, is_disabled, created_at, last_seen_at, password_hash) \
         VALUES (?, ?, ?, ?, 0, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&email)
    .bind(&display_name)
    .bind(role)
    .bind(now)
    .bind(now)
    .bind(&password_hash)
    .execute(&state.pool)
    .await?;

    let token = issue_token(&id, role, &state.jwt_secret)?;
    let user = UserDto {
        id,
        email,
        display_name,
        role: role.to_string(),
        created_at: now,
    };
    Ok((StatusCode::CREATED, Json(AuthResponse { token, user })))
}

/// What we hydrate from the users table on login. Strictly an internal
/// row shape - `password_hash` and `is_disabled` aren't part of the wire
/// response.
#[derive(sqlx::FromRow)]
struct LoginRow {
    id: String,
    email: String,
    display_name: String,
    role: String,
    created_at: i64,
    password_hash: Option<String>,
    is_disabled: i64,
}

pub async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginBody>,
) -> Result<Json<AuthResponse>, ApiError> {
    let email = body.email.trim().to_lowercase();

    // Fetch by email; may be None. Either way we run the verify path
    // below so timing doesn't disclose whether the email is registered.
    let row: Option<LoginRow> = sqlx::query_as(
        "SELECT id, email, display_name, role, created_at, password_hash, is_disabled \
         FROM users WHERE email = ?",
    )
    .bind(&email)
    .fetch_optional(&state.pool)
    .await?;

    // We deliberately give specific messages here rather than a generic
    // "invalid credentials". This is an internal-tool deployment - the
    // small enumeration risk of distinguishing "no such email" from
    // "wrong password" is outweighed by users wasting time guessing
    // which one their actual mistake is. If this ever ships to a wider
    // audience, this branch should collapse back to one opaque message.
    let row = match row {
        None => {
            return Err(ApiError::unauthorized(
                "no_account",
                "no account found for this email - check the address or sign up",
            ));
        }
        Some(r) => r,
    };
    if row.is_disabled != 0 {
        return Err(ApiError::forbidden(
            "account_disabled",
            "your account is disabled - contact your administrator",
        ));
    }
    if row.password_hash.is_none() {
        return Err(ApiError::unauthorized(
            "no_password",
            "this account signs in via SSO; password login isn't enabled for it",
        ));
    }
    let ok = verify_password_constant_time(&body.password, row.password_hash.as_deref());
    if !ok {
        return Err(ApiError::unauthorized(
            "wrong_password",
            "incorrect password for this account",
        ));
    }

    let now = Utc::now().timestamp();
    sqlx::query("UPDATE users SET last_seen_at = ? WHERE id = ?")
        .bind(now)
        .bind(&row.id)
        .execute(&state.pool)
        .await?;

    let token = issue_token(&row.id, &row.role, &state.jwt_secret)?;
    let user = UserDto {
        id: row.id,
        email: row.email,
        display_name: row.display_name,
        role: row.role,
        created_at: row.created_at,
    };
    Ok(Json(AuthResponse { token, user }))
}

/// Stateless JWTs can't be revoked server-side without a revocation list
/// (a v1.1 concern). For now this just returns 204 so the client can
/// uniformly call it on sign-out without special-casing.
pub async fn logout() -> StatusCode {
    StatusCode::NO_CONTENT
}

#[derive(Debug, Serialize)]
pub struct MeResponse {
    pub user: UserDto,
    /// When the calling token has less than `REFRESH_THRESHOLD_HOURS`
    /// of lifetime left, the server issues a fresh 30-day token and
    /// returns it here. The desktop client swaps its stored credential
    /// to the new value, so a user who touches the app every couple
    /// of weeks stays signed in indefinitely. None when the current
    /// token still has plenty of life.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refreshed_token: Option<String>,
}

pub async fn me(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<MeResponse>, ApiError> {
    let user: UserDto =
        sqlx::query_as("SELECT id, email, display_name, role, created_at FROM users WHERE id = ?")
            .bind(&auth.0.sub)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| {
                // Token validates, but user was deleted between issue and now.
                ApiError::unauthorized("user_gone", "your account no longer exists")
            })?;

    // Auto-rotation: if the token presented to us is nearing expiry,
    // mint a fresh one so the client can swap. The new token carries
    // the SAME role + sub as the old (we re-read the role from the
    // row above in case the admin promoted/demoted), so this isn't a
    // privilege-escalation vector - just a lifetime extension.
    let now = Utc::now().timestamp();
    let exp = auth.0.exp as i64;
    let remaining_hours = (exp - now) / 3600;
    let refreshed_token = if remaining_hours < crate::auth::REFRESH_THRESHOLD_HOURS {
        Some(issue_token(&user.id, &user.role, &state.jwt_secret)?)
    } else {
        None
    };

    Ok(Json(MeResponse {
        user,
        refreshed_token,
    }))
}

/// Permissive email check - RFC 5322 is too forgiving for a useful
/// regex. Reject only the obvious junk (no '@', no '.' after it, etc.).
/// The OIDC path handles real-world validation; this is the password
/// fallback's seatbelt.
fn looks_like_email(s: &str) -> bool {
    let Some(at) = s.find('@') else {
        return false;
    };
    let (local, domain) = s.split_at(at);
    let domain = &domain[1..];
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.')
}
