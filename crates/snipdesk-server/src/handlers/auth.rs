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

/// What `/api/auth/methods` returns: which sign-in surfaces the
/// running server is configured for. Unauthenticated by design so the
/// client can fetch it before the user has any credentials. The
/// desktop renders password fields + provider buttons strictly off
/// this response, which lets a single client binary serve both
/// password-only deployments and SSO-only ones without local config
/// guesswork.
#[derive(Debug, Serialize)]
pub struct AuthMethodsResponse {
    pub password: AuthMethodPassword,
    pub providers: Vec<AuthMethodProvider>,
}

#[derive(Debug, Serialize)]
pub struct AuthMethodPassword {
    /// True when the password endpoints (signup/login) are usable.
    /// Currently always true: the server has no config knob to
    /// disable them entirely. Reserved as a field so a future
    /// `[auth] password_enabled = false` can flip the client off
    /// without a protocol change.
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct AuthMethodProvider {
    /// Stable identifier used in the OIDC URL path. Currently
    /// `"google"` is the only value; `"keycloak"` joins once the
    /// generic OIDC refactor lands.
    pub id: String,
    /// Button label the client should render. Google's stays
    /// "Sign in with Google" per Google branding guidelines; other
    /// providers honour their config's `display_name` (or fall back
    /// to "Sign in with SSO").
    pub display_name: String,
    /// Where the client opens to kick off the OIDC flow. Server is
    /// the source of truth for the URL so a future per-provider
    /// rewrite (e.g. `/api/auth/oidc/<id>/start`) doesn't need a
    /// matching client change - the client just opens whatever the
    /// server tells it to.
    pub start_url: String,
}

/// GET /api/auth/methods - unauthenticated.
///
/// The client hits this when it needs to render its sign-in surface
/// (Settings -> Team Library, the onboarding sign-in step). The
/// response enumerates exactly which methods the server has
/// configured; the client builds its UI from that, no local guessing.
///
/// Keeping this endpoint unauthenticated is deliberate: by definition
/// the caller doesn't have credentials yet. The response leaks zero
/// information of value to an attacker - they're learning which
/// public OIDC providers a public server endpoint accepts, which is
/// also visible to anyone who tries to call those endpoints directly.
pub async fn methods(State(state): State<AppState>) -> Json<AuthMethodsResponse> {
    let mut providers = Vec::new();

    if state.oidc_google.is_some() {
        providers.push(AuthMethodProvider {
            id: "google".to_string(),
            // Hardcoded per Google identity branding guidelines: this
            // string and the Google logo are reserved phrasings.
            display_name: "Sign in with Google".to_string(),
            start_url: "/api/auth/oidc/start".to_string(),
        });
    }

    // Keycloak slot lights up once the generic OIDC refactor
    // (Keycloak step 3+) wires its per-provider start route. The
    // [oidc.keycloak] block already parses cleanly (step 2); the
    // handler that consumes it is the next major commit.

    Json(AuthMethodsResponse {
        password: AuthMethodPassword { enabled: true },
        providers,
    })
}

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
    /// Per-user words-per-minute override. None means "use the
    /// server's [stats] default" - the desktop Settings UI shows
    /// the default in a placeholder when the override is None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wpm: Option<i64>,
    /// Per-user hourly wage override in `currency`. None means "use
    /// the server's [stats] default".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hourly_wage: Option<f64>,
    /// ISO currency code the wage is expressed in. None means "use
    /// the server's [stats] default".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
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

    // Always hash the password before any DB write. The previous
    // SELECT-then-INSERT pre-check was a faster failure path for
    // already-registered emails but it leaked which emails exist via
    // a distinct `email_taken` / 409 response (CWE-203). It also
    // leaked via timing: the duplicate path was much faster than the
    // ~50ms Argon2 cost on a fresh signup (CWE-208). Hashing up
    // front and relying on the UNIQUE(email) index to reject
    // duplicates eliminates both leaks - every signup attempt pays
    // the same wall-clock cost.
    let password_hash = hash_password(&body.password)?;
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();

    // First-admin auto-promotion folded INTO the INSERT statement.
    // The CASE subquery runs under the same statement-level write
    // lock SQLite acquires for the INSERT itself, so two concurrent
    // signups on a fresh DB cannot both read admin_count=0 and both
    // land as admin (audit Tier 1 #6). Whichever INSERT lands first
    // sees count=0 and becomes the admin; the second sees count=1
    // (the first's committed row) and lands as member.
    //
    // RETURNING reads back the role the DB chose so we can issue the
    // matching JWT without doing a follow-up SELECT.
    //
    // UNIQUE(email) violations become a generic `signup_failed` (400)
    // for the same enumeration-prevention reasons as audit Tier 1 #3.
    // Any other DB error bubbles up as a 500 with the cause logged
    // server-side only.
    let inserted: Result<(String,), sqlx::Error> = sqlx::query_as(
        "INSERT INTO users (id, email, display_name, role, is_disabled, created_at, last_seen_at, password_hash) \
         VALUES ( \
           ?, ?, ?, \
           CASE WHEN (SELECT COUNT(*) FROM users WHERE role = 'admin') = 0 \
                THEN 'admin' \
                ELSE 'member' \
           END, \
           0, ?, ?, ? \
         ) \
         RETURNING role",
    )
    .bind(&id)
    .bind(&email)
    .bind(&display_name)
    .bind(now)
    .bind(now)
    .bind(&password_hash)
    .fetch_one(&state.pool)
    .await;

    let role = match inserted {
        Ok((r,)) => r,
        Err(sqlx::Error::Database(db_err))
            if db_err.kind() == sqlx::error::ErrorKind::UniqueViolation =>
        {
            return Err(ApiError::bad_request(
                "signup_failed",
                "signup failed; if you already have an account, sign in instead",
            ));
        }
        Err(e) => return Err(e.into()),
    };

    let token = issue_token(&id, &role, &state.jwt_secret)?;
    let user = UserDto {
        id,
        email,
        display_name,
        role,
        created_at: now,
        // Fresh signup: per-user wpm/wage/currency overrides start
        // unset; the dashboard falls back to the [stats] defaults
        // until the user configures their own numbers via PATCH
        // /api/me from the desktop settings panel.
        wpm: None,
        hourly_wage: None,
        currency: None,
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

    // Fetch by email; may be None. We run the verify path below
    // unconditionally so timing doesn't disclose whether the email is
    // registered (CWE-208).
    let row: Option<LoginRow> = sqlx::query_as(
        "SELECT id, email, display_name, role, created_at, password_hash, is_disabled \
         FROM users WHERE email = ?",
    )
    .bind(&email)
    .fetch_optional(&state.pool)
    .await?;

    // Always perform the Argon2 verify. When the row is missing or the
    // user has no password (SSO-only) the helper falls back to a
    // pre-computed SENTINEL_HASH, so the wall-clock cost is identical
    // whether the email exists, is disabled, is SSO-only, or has a
    // wrong password.
    let password_hash = row.as_ref().and_then(|r| r.password_hash.as_deref());
    let password_ok = verify_password_constant_time(&body.password, password_hash);

    // Collapse every failure mode (missing email, disabled, SSO-only,
    // wrong password) into one generic response. Differential responses
    // would let an attacker enumerate registered emails (CWE-203) -
    // valuable input for targeted brute force or phishing. The cost is
    // slightly worse UX for legitimate users with disabled or SSO-only
    // accounts: they receive the same opaque "invalid email or password"
    // and need to contact their administrator to learn why.
    let row = match row {
        Some(r) if r.is_disabled == 0 && r.password_hash.is_some() && password_ok => r,
        _ => {
            return Err(ApiError::unauthorized(
                "invalid_credentials",
                "invalid email or password",
            ));
        }
    };

    let now = Utc::now().timestamp();
    sqlx::query("UPDATE users SET last_seen_at = ? WHERE id = ?")
        .bind(now)
        .bind(&row.id)
        .execute(&state.pool)
        .await?;

    let token = issue_token(&row.id, &row.role, &state.jwt_secret)?;
    // Re-read the wage knobs in a tiny follow-up SELECT - LoginRow
    // intentionally doesn't carry them (keeps the password-hash row
    // narrow). Two queries on the happy path is fine for login,
    // which happens once per token lifetime.
    let extras: (Option<i64>, Option<f64>, Option<String>) =
        sqlx::query_as("SELECT wpm, hourly_wage, currency FROM users WHERE id = ?")
            .bind(&row.id)
            .fetch_one(&state.pool)
            .await?;
    let user = UserDto {
        id: row.id,
        email: row.email,
        display_name: row.display_name,
        role: row.role,
        created_at: row.created_at,
        wpm: extras.0,
        hourly_wage: extras.1,
        currency: extras.2,
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
    let user: UserDto = sqlx::query_as(
        "SELECT id, email, display_name, role, created_at, wpm, hourly_wage, currency \
         FROM users WHERE id = ?",
    )
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

/// Body of `PATCH /api/me`. Every field is optional; a `Some(None)`
/// value (`null` over the wire) clears the override and reverts that
/// dimension to the server-wide [stats] default. A missing field
/// leaves the column untouched.
#[derive(Debug, Deserialize)]
pub struct UpdateMeBody {
    /// Words-per-minute. Sanity bounds: 1..=500. Outside that range
    /// we 400 rather than silently clamping; the desktop UI should
    /// catch this before sending.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub wpm: Option<Option<i64>>,
    /// Hourly wage in `currency`. Must be > 0 when present.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub hourly_wage: Option<Option<f64>>,
    /// ISO currency code. Validated against the server's aud_rates
    /// table; an unknown code 400s so users can't accidentally pin
    /// themselves to a 1:1 fallback.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub currency: Option<Option<String>>,
}

/// Helper to deserialize "field is absent" vs "field is present as
/// null". The default serde flavour collapses both to `None`; we
/// need to distinguish "leave alone" from "clear to default".
fn deserialize_some<'de, T, D>(d: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(d).map(Some)
}

/// PATCH /api/me. Updates the per-user wpm/hourly_wage/currency
/// overrides used by the admin dashboard's hours/money saved
/// estimates. Returns the refreshed user row so the desktop can
/// confirm the new state without a follow-up GET.
pub async fn update_me(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<UpdateMeBody>,
) -> Result<Json<UserDto>, ApiError> {
    // Validate first so we never half-update on a bogus payload.
    if let Some(Some(v)) = body.wpm {
        if !(1..=500).contains(&v) {
            return Err(ApiError::bad_request(
                "invalid_wpm",
                "wpm must be between 1 and 500",
            ));
        }
    }
    if let Some(Some(v)) = body.hourly_wage {
        if !v.is_finite() || v <= 0.0 || v > 100_000.0 {
            return Err(ApiError::bad_request(
                "invalid_wage",
                "hourly_wage must be positive and finite",
            ));
        }
    }
    if let Some(Some(ref c)) = body.currency {
        // Accept any ISO-ish 3-letter code that we know how to
        // convert. The static aud_rates table is the source of
        // truth; live-FX (when wired up) just refreshes the same
        // map in place.
        if !state.stats.aud_rates.contains_key(c) {
            return Err(ApiError::bad_request(
                "unknown_currency",
                format!("currency '{c}' is not in the server's aud_rates table"),
            ));
        }
    }

    // Build the UPDATE dynamically only for fields the caller
    // actually sent. SQLite doesn't have a native "SET x = COALESCE(?,
    // x)" pattern that would let us collapse this to one statement
    // without losing the "set to NULL" semantics, so we string
    // together a small SET clause.
    let mut sets: Vec<&str> = Vec::new();
    if body.wpm.is_some() {
        sets.push("wpm = ?");
    }
    if body.hourly_wage.is_some() {
        sets.push("hourly_wage = ?");
    }
    if body.currency.is_some() {
        sets.push("currency = ?");
    }

    if !sets.is_empty() {
        let sql = format!("UPDATE users SET {} WHERE id = ?", sets.join(", "));
        let mut q = sqlx::query(&sql);
        if let Some(v) = body.wpm {
            q = q.bind(v);
        }
        if let Some(v) = body.hourly_wage {
            q = q.bind(v);
        }
        if let Some(v) = body.currency {
            q = q.bind(v);
        }
        q = q.bind(&auth.0.sub);
        q.execute(&state.pool).await?;
    }

    let user: UserDto = sqlx::query_as(
        "SELECT id, email, display_name, role, created_at, wpm, hourly_wage, currency \
         FROM users WHERE id = ?",
    )
    .bind(&auth.0.sub)
    .fetch_one(&state.pool)
    .await?;
    Ok(Json(user))
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
