//! Multi-provider OIDC sign-in (Google Workspace + Keycloak).
//!
//! Two-step browser dance, identical shape across providers:
//!   1. `start` - the desktop client opens this in the user's browser.
//!      We generate a CSRF state, a PKCE verifier, and a nonce, stash
//!      all three keyed by state (alongside which provider was picked),
//!      then 302 the browser to the provider's authorize endpoint.
//!   2. `callback` - the provider redirects here after the user signs
//!      in. We look up the stored state, exchange the code for tokens,
//!      verify the ID token, run provider-specific checks (Google's
//!      `hd` claim or Keycloak's realm-role check), find-or-create
//!      the local user, and issue our own HS256 JWT. The response is
//!      an HTML page that fires a `<scheme>://auth?token=...` deep
//!      link AND exposes the token for manual copy as a fallback.
//!
//! Per-provider behaviour rides on `Provider` (Google / Keycloak).
//! The shared `start_flow` / `complete_flow` functions stay
//! provider-agnostic; the variant-specific bits (issuer URL, extra
//! claim checks, admin-role mapping, button label) hang off methods
//! on the enum.
//!
//! State store: in-memory `HashMap<state, PendingAuth>` behind a
//! Mutex. Entries expire after 10 minutes; each request prunes
//! expired entries before doing its own work. For a v1 single-process
//! deployment this is fine. A multi-instance deploy would need a
//! shared store (Redis), but that's a v2 concern.
//!
//! User-facing error strings are deliberately opaque ("sign-in
//! failed"). The underlying token-exchange / id-token-verify failure
//! is captured via `tracing::warn!` with full detail server-side, so
//! operators can debug from logs without leaking the exact failure
//! mode to whoever just hit the callback URL.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::Utc;
use once_cell::sync::Lazy;
use openidconnect::core::{
    CoreAuthenticationFlow, CoreClient, CoreIdTokenClaims, CoreProviderMetadata,
};
use openidconnect::reqwest::async_http_client;
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use axum_extra::extract::cookie::CookieJar;

use crate::audit::{self, action, AuditEvent};
use crate::auth::issue_token;
use crate::config::{GoogleOidcConfig, KeycloakOidcConfig};
use crate::dashboard::session::build_cookie;
use crate::error::ApiError;
use crate::http::AppState;

/// Which provider a flow belongs to. Drives every per-provider
/// branch (issuer URL, extra claim validation, admin-role mapping,
/// the string stamped into `users.oidc_provider`). Keep the variant
/// list tight; adding a third provider means adding the matching
/// `[oidc.<name>]` config block, an `AppState` slot, and matching
/// branches in the methods on this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    Google,
    Keycloak,
}

impl Provider {
    /// Stable string used in URL paths (`/api/auth/oidc/<id>/start`)
    /// and stored in `users.oidc_provider`. Lowercase ASCII only;
    /// don't rename a variant once it's gone live - existing user
    /// rows reference it.
    fn id(self) -> &'static str {
        match self {
            Provider::Google => "google",
            Provider::Keycloak => "keycloak",
        }
    }

    /// Parse a path segment back into a Provider. Returns None for
    /// unknown names so the route handler can 404 cleanly.
    fn from_id(s: &str) -> Option<Provider> {
        match s {
            "google" => Some(Provider::Google),
            "keycloak" => Some(Provider::Keycloak),
            _ => None,
        }
    }
}

/// Borrowed view of a provider's config. `start_flow` and
/// `complete_flow` work against this so they don't have to know which
/// concrete struct backs the running provider.
enum ProviderConfig<'a> {
    Google(&'a GoogleOidcConfig),
    Keycloak(&'a KeycloakOidcConfig),
}

impl ProviderConfig<'_> {
    fn provider(&self) -> Provider {
        match self {
            ProviderConfig::Google(_) => Provider::Google,
            ProviderConfig::Keycloak(_) => Provider::Keycloak,
        }
    }

    fn client_id(&self) -> &str {
        match self {
            ProviderConfig::Google(g) => &g.client_id,
            ProviderConfig::Keycloak(k) => &k.client_id,
        }
    }

    fn client_secret(&self) -> &str {
        match self {
            ProviderConfig::Google(g) => &g.client_secret,
            ProviderConfig::Keycloak(k) => &k.client_secret,
        }
    }

    fn redirect_uri(&self) -> &str {
        match self {
            ProviderConfig::Google(g) => &g.redirect_uri,
            ProviderConfig::Keycloak(k) => &k.redirect_uri,
        }
    }

    /// Issuer URL the openidconnect crate hits for the discovery
    /// document. Google's is fixed; Keycloak's is per-realm and
    /// comes from the config.
    fn issuer_url(&self) -> &str {
        match self {
            ProviderConfig::Google(_) => "https://accounts.google.com",
            ProviderConfig::Keycloak(k) => &k.issuer_url,
        }
    }

    /// Soft email-domain allowlist. Empty list means no filter.
    fn allowed_email_domains(&self) -> &[String] {
        match self {
            ProviderConfig::Google(g) => &g.allowed_email_domains,
            ProviderConfig::Keycloak(k) => &k.allowed_email_domains,
        }
    }
}

/// Look up the configured provider on AppState. Returns the opaque
/// "sign-in failed" error to the caller when the slot is empty; the
/// log line carries the exact "oidc <provider> not configured"
/// detail so operators see it without it being exposed to users.
fn provider_config<'a>(
    state: &'a AppState,
    provider: Provider,
) -> Result<ProviderConfig<'a>, ApiError> {
    match provider {
        Provider::Google => state
            .oidc_google
            .as_ref()
            .map(ProviderConfig::Google)
            .ok_or_else(|| {
                tracing::warn!("oidc start/callback for google but [oidc.google] is unset");
                generic_signin_failed()
            }),
        Provider::Keycloak => state
            .oidc_keycloak
            .as_ref()
            .map(ProviderConfig::Keycloak)
            .ok_or_else(|| {
                tracing::warn!("oidc start/callback for keycloak but [oidc.keycloak] is unset");
                generic_signin_failed()
            }),
    }
}

/// A pending authorization waiting for its callback. The keys we
/// need to keep alive between /start and /callback: PKCE verifier
/// (proves the same user agent initiated both calls), nonce (binds
/// the ID token to this specific authorization), and the flow's
/// completion strategy (desktop deep-link vs dashboard cookie).
/// `provider` tags the entry so `callback` knows which config +
/// validation hooks to load.
struct PendingAuth {
    provider: Provider,
    pkce_verifier: PkceCodeVerifier,
    nonce: Nonce,
    flow: FlowOrigin,
    created_at: i64,
}

/// How the OIDC flow should land once we have a verified user. The
/// IdP-side callback URL is the same for both - the divergence is
/// after the local user_id is known. Desktop hands off via the
/// deep-link landing page (with paste-token fallback); Dashboard
/// sets the session cookie and bounces to a same-origin page.
#[derive(Clone)]
enum FlowOrigin {
    /// Desktop client. `client_redirect` is the `<scheme>://auth`
    /// URL the callback page will deep-link into.
    Desktop { client_redirect: String },
    /// Dashboard browser session. `redirect_to` is the same-origin
    /// path to bounce the browser to once the cookie is set; the
    /// caller already ran it through `safe_next` so it can't be an
    /// off-host open-redirect.
    Dashboard { redirect_to: String },
}

/// 10-minute TTL on pending auths. Long enough for a user to actually
/// sign in (including any 2FA prompts), short enough that abandoned
/// state doesn't accumulate forever.
const PENDING_AUTH_TTL_SECS: i64 = 600;

fn pending_store() -> &'static Mutex<HashMap<String, PendingAuth>> {
    static STORE: Lazy<Mutex<HashMap<String, PendingAuth>>> =
        Lazy::new(|| Mutex::new(HashMap::new()));
    &STORE
}

/// Drop entries older than the TTL. Called at the start of every
/// OIDC endpoint so cleanup is cheap (a quick HashMap walk) and
/// runs implicitly with traffic - no separate sweep task needed.
fn prune_pending(now: i64) {
    if let Ok(mut store) = pending_store().lock() {
        store.retain(|_, p| now - p.created_at < PENDING_AUTH_TTL_SECS);
    }
}

/// Cached provider metadata + the JWKS bundled with it. Saves the
/// ~150 ms discovery round-trip on every sign-in. The TTL is
/// deliberately conservative (1 hour) - JWKS can rotate roughly
/// daily and the openidconnect crate validates against whatever
/// metadata we hand the client, so a stale cache could reject a
/// freshly-rotated key. 1 hour bounds that window without paying
/// the discovery cost every single login. Keyed by issuer URL so
/// each provider gets its own cache slot.
const METADATA_TTL_SECS: i64 = 3600;

struct CachedMetadata {
    metadata: CoreProviderMetadata,
    expires_at: i64,
}

fn metadata_cache() -> &'static Mutex<HashMap<String, CachedMetadata>> {
    static CACHE: Lazy<Mutex<HashMap<String, CachedMetadata>>> =
        Lazy::new(|| Mutex::new(HashMap::new()));
    &CACHE
}

/// Return cached provider metadata, fetching only when the slot is
/// empty or expired. Releases the lock before any await so we can't
/// deadlock by holding a std::sync::Mutex across `.await`.
async fn cached_metadata(issuer: &str) -> Result<CoreProviderMetadata, ApiError> {
    let now = Utc::now().timestamp();
    {
        let cache = metadata_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = cache.get(issuer) {
            if c.expires_at > now {
                return Ok(c.metadata.clone());
            }
        }
    }
    let issuer_url = IssuerUrl::new(issuer.to_string()).map_err(|e| {
        tracing::warn!(issuer = %issuer, "oidc issuer url parse failed: {e}");
        generic_signin_failed()
    })?;
    let metadata = CoreProviderMetadata::discover_async(issuer_url, async_http_client)
        .await
        .map_err(|e| {
            tracing::warn!(issuer = %issuer, "oidc discovery failed: {e}");
            generic_signin_failed()
        })?;
    {
        let mut cache = metadata_cache().lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(
            issuer.to_string(),
            CachedMetadata {
                metadata: metadata.clone(),
                expires_at: now + METADATA_TTL_SECS,
            },
        );
    }
    Ok(metadata)
}

/// Build the openidconnect Client for the active provider. Metadata
/// is served from a 1-hour in-process cache so steady-state sign-ins
/// don't pay the discovery round-trip.
async fn build_client(cfg: &ProviderConfig<'_>) -> Result<CoreClient, ApiError> {
    let metadata = cached_metadata(cfg.issuer_url()).await?;
    let redirect = RedirectUrl::new(cfg.redirect_uri().to_string()).map_err(|e| {
        tracing::warn!(
            provider = %cfg.provider().id(),
            "oidc redirect uri parse failed: {e}"
        );
        generic_signin_failed()
    })?;
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(cfg.client_id().to_string()),
        Some(ClientSecret::new(cfg.client_secret().to_string())),
    )
    .set_redirect_uri(redirect);
    Ok(client)
}

#[derive(Debug, Deserialize)]
pub struct StartQuery {
    /// Where the callback should send the user after issuing a JWT.
    /// Defaults to the first scheme in
    /// `[oidc].allowed_deep_link_schemes` (typically `snipdesk://auth`).
    /// Each value is checked against that allowlist so this can't
    /// become an open redirector.
    #[serde(default)]
    pub redirect: Option<String>,
}

/// Pick the URL the callback page should attempt to deep-link the
/// browser into. Accepts the client-supplied `?redirect=<scheme>://auth`
/// when its scheme is in the allowlist; otherwise falls back to the
/// first configured scheme. Empty allowlist (shouldn't happen, but
/// the config default keeps it populated) falls back to "snipdesk".
fn resolve_client_redirect(requested: Option<&str>, allowed: &[String]) -> String {
    let default_scheme = allowed.first().map(String::as_str).unwrap_or("snipdesk");
    if let Some(s) = requested {
        // Pull the scheme out of "<scheme>://...": the allowlist
        // compares scheme strings, not the full URL.
        if let Some(idx) = s.find("://") {
            let scheme = &s[..idx];
            if !scheme.is_empty() && allowed.iter().any(|a| a == scheme) {
                return s.to_string();
            }
        }
    }
    format!("{default_scheme}://auth")
}

/// Generic "sign-in failed" error returned to the user when anything
/// inside the OIDC flow goes wrong. Keeps the wire response opaque -
/// the operator-facing detail goes into the surrounding `tracing`
/// call. Used for both 4xx and "things-that-could-be-attacker-probes"
/// 4xx paths; 500-level failures still come back as ApiError::internal
/// so the existing logging in error.rs catches them.
fn generic_signin_failed() -> ApiError {
    ApiError::bad_request(
        "signin_failed",
        "sign-in failed; please try again or contact your administrator",
    )
}

// ---------------------------------------------------------------------
// Public HTTP entry points
//
// These are thin shells over `start_flow` / `complete_flow`. The
// per-provider routes (`/api/auth/oidc/:provider/...`) land in
// step 4; for now the legacy Google-only `/api/auth/oidc/start` and
// `/callback` shims stay so existing clients keep working.
// ---------------------------------------------------------------------

pub async fn start(
    State(state): State<AppState>,
    Query(q): Query<StartQuery>,
) -> Result<Response, ApiError> {
    let flow = desktop_flow_from_query(&state, q.redirect.as_deref());
    start_flow(state, Provider::Google, flow).await
}

pub async fn callback(
    state: State<AppState>,
    q: Query<CallbackQuery>,
) -> Result<Response, ApiError> {
    complete_flow(state, q).await
}

/// Per-provider start: `/api/auth/oidc/:provider/start`. Resolves
/// the provider segment to a Provider variant and delegates to the
/// shared core. Unknown provider names render the same opaque
/// error page as a misconfigured provider so a probing attacker
/// can't easily enumerate which IdPs the operator has enabled
/// (the public `/api/auth/methods` endpoint is the canonical
/// answer to that question; this URL is purposeful machine input).
pub async fn start_provider(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(q): Query<StartQuery>,
) -> Result<Response, ApiError> {
    let provider = Provider::from_id(&provider).ok_or_else(|| {
        tracing::warn!(provider = %provider, "oidc start with unknown provider id");
        generic_signin_failed()
    })?;
    let flow = desktop_flow_from_query(&state, q.redirect.as_deref());
    start_flow(state, provider, flow).await
}

/// Per-provider callback: `/api/auth/oidc/:provider/callback`. The
/// provider segment is informational at this point - the pending
/// auth entry the state pointed at already records which provider
/// the flow belongs to, so the URL slug just keeps the redirect URI
/// registered with each IdP human-readable. We still resolve it so
/// the route handler 404s on garbage like `/api/auth/oidc/<>/callback`.
pub async fn callback_provider(
    state: State<AppState>,
    Path(provider): Path<String>,
    q: Query<CallbackQuery>,
) -> Result<Response, ApiError> {
    if Provider::from_id(&provider).is_none() {
        tracing::warn!(provider = %provider, "oidc callback with unknown provider id");
        return Err(generic_signin_failed());
    }
    complete_flow(state, q).await
}

/// Dashboard-side entry point. The dashboard module mounts this on
/// `/dashboard/oidc/:provider/start?redirect_to=...`. The IdP's
/// registered callback URL stays the same (`/api/auth/oidc/:provider/callback`);
/// only the in-memory PendingAuth differs, carrying a
/// `FlowOrigin::Dashboard` that the shared completion path uses
/// to set the session cookie + bounce instead of rendering the
/// desktop deep-link page. `provider` is opaque to callers - they
/// resolve it via [`provider_from_id`] first.
pub async fn dashboard_start(
    state: AppState,
    provider: ProviderHandle,
    redirect_to: String,
) -> Result<Response, ApiError> {
    let flow = FlowOrigin::Dashboard { redirect_to };
    start_flow(state, provider.0, flow).await
}

/// Opaque wrapper around the internal Provider enum. Lets the
/// dashboard module hold a Provider value without the enum itself
/// having to be public. The only way to build one is via
/// [`provider_from_id`], which keeps validation in this module.
pub struct ProviderHandle(Provider);

/// Resolve a provider id (`"google"`, `"keycloak"`) to a handle the
/// dashboard module can pass back into [`dashboard_start`]. Returns
/// None for unknown ids; the caller bounces with a generic error.
pub fn provider_from_id(s: &str) -> Option<ProviderHandle> {
    Provider::from_id(s).map(ProviderHandle)
}

/// Build the desktop FlowOrigin from the optional ?redirect= query
/// param. Pulled out so both the legacy unscoped start and the new
/// per-provider start handler agree on the resolution rule.
fn desktop_flow_from_query(state: &AppState, requested: Option<&str>) -> FlowOrigin {
    FlowOrigin::Desktop {
        client_redirect: resolve_client_redirect(requested, &state.oidc_allowed_schemes),
    }
}

// ---------------------------------------------------------------------
// Provider-agnostic core
// ---------------------------------------------------------------------

/// Kick off the OIDC dance for `provider`. Generates state + PKCE +
/// nonce, stashes them (along with the caller's chosen FlowOrigin)
/// keyed by state, returns a 302 to the provider's authorize
/// endpoint.
async fn start_flow(
    state_app: AppState,
    provider: Provider,
    flow: FlowOrigin,
) -> Result<Response, ApiError> {
    let cfg = provider_config(&state_app, provider)?;
    let now = Utc::now().timestamp();
    prune_pending(now);

    let client = build_client(&cfg).await?;

    // PKCE verifier + challenge. We send the challenge to the
    // provider in the authorize step; the verifier comes back to us
    // in the callback's token exchange. Without this an attacker who
    // intercepts the auth code can't redeem it.
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let (auth_url, csrf_state, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".to_string()))
        .add_scope(Scope::new("profile".to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    if let Ok(mut store) = pending_store().lock() {
        // Bound the store so an attacker hammering /api/auth/oidc/start
        // without ever finishing the flow can't exhaust memory. 1024
        // pending entries is well above any plausible legitimate burst
        // (an org of 10k users with everyone simultaneously signing in
        // would still average <0.1s in this state given the TTL). If
        // we're already at the cap, drop the oldest entry to make room
        // - the rare legitimate user whose pending state expires this
        // way just has to click "Sign in" again.
        const PENDING_CAP: usize = 1024;
        if store.len() >= PENDING_CAP {
            if let Some(oldest_key) = store
                .iter()
                .min_by_key(|(_, p)| p.created_at)
                .map(|(k, _)| k.clone())
            {
                store.remove(&oldest_key);
                tracing::warn!(
                    "oidc pending_store hit cap ({}); dropped oldest entry",
                    PENDING_CAP
                );
            }
        }
        store.insert(
            csrf_state.secret().to_string(),
            PendingAuth {
                provider,
                pkce_verifier,
                nonce,
                flow,
                created_at: now,
            },
        );
    }

    Ok(Redirect::to(auth_url.as_str()).into_response())
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// The provider redirects here after the user signs in (or errors
/// out). We exchange the code for an ID token, verify the token,
/// run provider-specific claim checks, find or create the matching
/// local user, and respond with an HTML page that fires the desktop
/// deep link plus a copy-paste fallback.
async fn complete_flow(
    State(state_app): State<AppState>,
    Query(q): Query<CallbackQuery>,
) -> Result<Response, ApiError> {
    if let Some(err) = q.error.as_deref() {
        tracing::warn!(
            error = err,
            detail = q.error_description.as_deref().unwrap_or(""),
            "oidc provider returned error to callback"
        );
        // Opaque user-facing message; the provider's exact code +
        // description is in the log line above for the operator.
        return Ok(render_callback_error(
            "Sign-in was cancelled or declined. Close this tab and try again from SnipDesk.",
        ));
    }

    let code = q.code.as_deref().ok_or_else(|| {
        tracing::warn!("oidc callback without a code");
        generic_signin_failed()
    })?;
    let state = q.state.as_deref().ok_or_else(|| {
        tracing::warn!("oidc callback without a state");
        generic_signin_failed()
    })?;

    let now = Utc::now().timestamp();
    prune_pending(now);

    let pending = pending_store()
        .lock()
        .ok()
        .and_then(|mut s| s.remove(state))
        .ok_or_else(|| {
            // Either the state was never issued (attacker probe) or
            // it expired. Log without the actual state value (don't
            // want it in logs as a foothold for replay) and bounce.
            tracing::warn!("oidc callback with unknown or expired state");
            generic_signin_failed()
        })?;

    let cfg = provider_config(&state_app, pending.provider)?;
    let client = build_client(&cfg).await?;

    let token_response = client
        .exchange_code(AuthorizationCode::new(code.to_string()))
        .set_pkce_verifier(pending.pkce_verifier)
        .request_async(async_http_client)
        .await
        .map_err(|e| {
            tracing::warn!(
                provider = %pending.provider.id(),
                "oidc token exchange failed: {e}"
            );
            generic_signin_failed()
        })?;

    let id_token = token_response.id_token().ok_or_else(|| {
        tracing::warn!(
            provider = %pending.provider.id(),
            "oidc token response missing id_token"
        );
        generic_signin_failed()
    })?;

    // Verify the ID token: signature via the provider's JWKS,
    // audience matches our client_id, issuer matches, nonce matches
    // the one we generated.
    let id_token_verifier = client.id_token_verifier();
    let claims = id_token
        .claims(&id_token_verifier, &pending.nonce)
        .map_err(|e| {
            tracing::warn!(
                provider = %pending.provider.id(),
                "oidc id_token verification failed: {e}"
            );
            generic_signin_failed()
        })?;

    // Provider-specific claim checks. Errors here are user-actionable
    // (wrong domain, missing role) so we surface a slightly more
    // informative message than the generic one - the operator
    // already saw the underlying claim mismatch in the log line.
    if let Err(rendered) = run_provider_checks(&cfg, claims, &id_token.to_string()) {
        return Ok(rendered);
    }

    let email = claims
        .email()
        .ok_or_else(|| {
            tracing::warn!(
                provider = %pending.provider.id(),
                "oidc id_token missing email claim"
            );
            generic_signin_failed()
        })?
        .as_str()
        .to_lowercase();

    if !cfg.allowed_email_domains().is_empty() {
        let domain = email.split('@').nth(1).unwrap_or("");
        if !cfg
            .allowed_email_domains()
            .iter()
            .any(|d| d.eq_ignore_ascii_case(domain))
        {
            tracing::warn!(
                provider = %pending.provider.id(),
                email_domain = %domain,
                "oidc sign-in rejected: email domain not in allowlist"
            );
            return Ok(render_callback_error(
                "Your email domain isn't in this server's allowlist.",
            ));
        }
    }

    let display_name = claims
        .name()
        .and_then(|m| m.get(None))
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|| email.split('@').next().unwrap_or("user").to_string());

    let subject = claims.subject().as_str().to_string();

    // Find-or-create. The hook may also flip an existing user's
    // admin/member status when the provider says so (Keycloak only).
    let admin_override = admin_role_present(&cfg, &id_token.to_string());
    let user_id = upsert_oidc_user(
        &state_app,
        pending.provider,
        &subject,
        &email,
        &display_name,
        admin_override,
    )
    .await?;

    // Re-read the user's role for the token. We just upserted; this
    // round-trip is cheap and avoids tracking the role through
    // upsert_oidc_user's return type.
    let role: String = sqlx::query_scalar("SELECT role FROM users WHERE id = ?")
        .bind(&user_id)
        .fetch_one(&state_app.pool)
        .await?;

    let snipdesk_token = issue_token(&user_id, &role, &state_app.jwt_secret)?;
    let now = Utc::now().timestamp();
    let _ = sqlx::query("UPDATE users SET last_seen_at = ? WHERE id = ?")
        .bind(now)
        .bind(&user_id)
        .execute(&state_app.pool)
        .await;

    // Dispatch on the flow origin we stashed at /start time.
    // Desktop: hand the token back via the deep-link HTML page so
    // the Tauri client picks it up over its URL-scheme handler.
    // Dashboard: set the same HttpOnly cookie the password login
    // would, and 302 the browser to the dashboard the user was
    // trying to reach.
    match pending.flow {
        FlowOrigin::Desktop { client_redirect } => Ok(render_callback_success(
            &client_redirect,
            &snipdesk_token,
            &email,
        )),
        FlowOrigin::Dashboard { redirect_to } => {
            // The cookie's Secure attribute follows the operator's
            // config (mirrors the password-login path's behaviour).
            // Non-admin members get bounced after the cookie sets;
            // members_blocked page lives in dashboard/session and
            // fires from the DashboardAdmin extractor on the next
            // request, so we don't have to special-case it here.
            let cookie = build_cookie(snipdesk_token, state_app.secure_cookies);
            let jar = CookieJar::new().add(cookie);
            Ok((jar, Redirect::to(&redirect_to)).into_response())
        }
    }
}

/// Per-provider claim validation that runs after the ID token is
/// signature-verified but before we trust any of the claims. Returns
/// `Err(Response)` carrying a user-facing HTML error page when the
/// check fails (the operator-facing detail is logged inside).
fn run_provider_checks(
    cfg: &ProviderConfig<'_>,
    claims: &CoreIdTokenClaims,
    id_token_jwt: &str,
) -> Result<(), Response> {
    match cfg {
        ProviderConfig::Google(g) => {
            // Workspace lockdown: `hd` claim must equal required_hd
            // when set. Google sets `hd` only for Workspace-managed
            // accounts; personal @gmail.com accounts have no hd. We
            // read it from the raw JWT payload because openidconnect's
            // typed claims default to EmptyAdditionalClaims; defining
            // a custom CoreClient type just to read one field is more
            // boilerplate than it's worth. The signature was already
            // verified above, so the payload bytes are trustworthy.
            if let Some(required) = g.required_hd.as_deref() {
                let hd_claim = read_string_claim(id_token_jwt, "hd").unwrap_or_default();
                if hd_claim != required {
                    tracing::warn!(
                        provider = "google",
                        expected_hd = %required,
                        actual_hd = %hd_claim,
                        "google sign-in rejected: hd claim mismatch"
                    );
                    return Err(render_callback_error(&format!(
                        "This server is locked to the {required} Workspace. \
                         Sign in with a {required} account."
                    )));
                }
            }
            // Suppress the unused-claims warning for the Google
            // branch; we don't currently read anything off `claims`
            // here, but keeping the parameter uniform across
            // providers lets future Google-side checks land without
            // re-threading the signature.
            let _ = claims;
        }
        ProviderConfig::Keycloak(k) => {
            // Realm-role check: when `required_realm_role` is set,
            // the ID token must list the role inside
            // `realm_access.roles`. We read the array off the raw
            // JWT payload for the same "openidconnect typed claims
            // don't surface this" reason as Google's hd above.
            if let Some(required) = k.required_realm_role.as_deref() {
                let has_role = realm_roles_from_jwt(id_token_jwt)
                    .iter()
                    .any(|r| r == required);
                if !has_role {
                    tracing::warn!(
                        provider = "keycloak",
                        required_role = %required,
                        "keycloak sign-in rejected: required realm role missing"
                    );
                    return Err(render_callback_error(
                        "Your account doesn't have access to this application. \
                         Contact your administrator if you think this is wrong.",
                    ));
                }
            }
            let _ = claims;
        }
    }
    Ok(())
}

/// Returns `Some(true)` when the provider's admin-role mapping is
/// configured AND present on the user's token, `Some(false)` when
/// the mapping is configured AND absent (the user should be demoted
/// on this sign-in), `None` when no admin mapping is configured for
/// this provider (admin status managed exclusively from the
/// dashboard / CLI for this user).
fn admin_role_present(cfg: &ProviderConfig<'_>, id_token_jwt: &str) -> Option<bool> {
    match cfg {
        ProviderConfig::Google(_) => None,
        ProviderConfig::Keycloak(k) => {
            let role = k.admin_role.as_deref()?;
            let present = realm_roles_from_jwt(id_token_jwt).iter().any(|r| r == role);
            Some(present)
        }
    }
}

/// Insert-or-update the user matching this `(provider, subject)`
/// pair. Returns the resulting user's id.
///
/// `admin_override` from the provider check (Keycloak only):
///   - `Some(true)` sets role = 'admin' on this user.
///   - `Some(false)` sets role = 'member'. For an existing row this
///     means demotion; for a new row it just pins the initial role
///     against the auto-promotion fallback.
///   - `None` leaves the role alone (Google's case, plus Keycloak
///     when no `admin_role` is configured).
async fn upsert_oidc_user(
    state: &AppState,
    provider: Provider,
    subject: &str,
    email: &str,
    display_name: &str,
    admin_override: Option<bool>,
) -> Result<String, ApiError> {
    let mut tx = state.pool.begin().await?;
    let provider_id = provider.id();

    // First: existing user already linked to this provider/subject.
    // Match on both columns so a future drop of the inline UNIQUE on
    // `oidc_subject` can be done without changing this query.
    if let Some((id, is_disabled)) = sqlx::query_as::<_, (String, i64)>(
        "SELECT id, is_disabled FROM users \
         WHERE oidc_subject = ? AND oidc_provider = ?",
    )
    .bind(subject)
    .bind(provider_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        if is_disabled != 0 {
            tracing::warn!(
                user_id = %id,
                provider = %provider_id,
                "oidc sign-in blocked: account is disabled"
            );
            // Keep the user-facing message generic so we don't tell
            // a probing attacker which emails are disabled. The
            // operator sees the cause in the log above.
            return Err(generic_signin_failed());
        }
        // Refresh display name in case it changed upstream.
        sqlx::query("UPDATE users SET display_name = ? WHERE id = ?")
            .bind(display_name)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        // Apply provider-driven admin override on each sign-in so
        // losing the admin role upstream demotes immediately.
        if let Some(want_admin) = admin_override {
            let new_role = if want_admin { "admin" } else { "member" };
            sqlx::query("UPDATE users SET role = ? WHERE id = ?")
                .bind(new_role)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        return Ok(id);
    }

    // Second: existing password account with the same email AND no
    // prior OIDC link. Linking is allowed only when the row is
    // currently provider-less; if it already has an oidc_provider
    // we refuse the merge silently (don't let a Keycloak token claim
    // an account already linked to Google by email, and vice versa).
    if let Some((id, is_disabled, existing_provider)) =
        sqlx::query_as::<_, (String, i64, Option<String>)>(
            "SELECT id, is_disabled, oidc_provider FROM users WHERE email = ?",
        )
        .bind(email)
        .fetch_optional(&mut *tx)
        .await?
    {
        if is_disabled != 0 {
            tracing::warn!(
                user_id = %id,
                "oidc sign-in blocked: account with this email is disabled"
            );
            return Err(generic_signin_failed());
        }
        if let Some(prev) = existing_provider {
            if prev != provider_id {
                tracing::warn!(
                    user_id = %id,
                    existing_provider = %prev,
                    attempted_provider = %provider_id,
                    "oidc sign-in blocked: email already linked to a different provider"
                );
                return Err(generic_signin_failed());
            }
            // Same provider, different subject? That shouldn't
            // happen in practice (providers don't reassign subs) but
            // refuse rather than silently rebind.
            tracing::warn!(
                user_id = %id,
                provider = %provider_id,
                "oidc sign-in blocked: email already linked under a different subject"
            );
            return Err(generic_signin_failed());
        }
        // Fresh link: the row had no OIDC info yet.
        sqlx::query(
            "UPDATE users SET oidc_subject = ?, oidc_provider = ?, display_name = ? \
             WHERE id = ?",
        )
        .bind(subject)
        .bind(provider_id)
        .bind(display_name)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        if let Some(want_admin) = admin_override {
            let new_role = if want_admin { "admin" } else { "member" };
            sqlx::query("UPDATE users SET role = ? WHERE id = ?")
                .bind(new_role)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        tracing::info!(
            user_id = %id,
            provider = %provider_id,
            email = %email,
            "oidc linked existing password account"
        );
        return Ok(id);
    }

    // Third: brand-new user. The new role is:
    //   - 'admin' when this is the first user in the org (same
    //     auto-promotion the password signup flow does), OR the
    //     provider says so via admin_override.
    //   - 'member' otherwise.
    //
    // Note: we use the atomic INSERT-with-CASE pattern that the
    // password signup path uses to close the first-admin race
    // (audit Tier 1 #6). Two concurrent OIDC signups can't both
    // observe admin_count = 0 because SQLite serialises the entire
    // INSERT under one write lock.
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();
    let forced_admin = admin_override.unwrap_or(false);
    sqlx::query(
        "INSERT INTO users \
           (id, email, display_name, role, is_disabled, \
            created_at, last_seen_at, oidc_subject, oidc_provider) \
         VALUES ( \
           ?, ?, ?, \
           CASE \
             WHEN ?5 = 1 THEN 'admin' \
             WHEN (SELECT COUNT(*) FROM users WHERE role = 'admin') = 0 THEN 'admin' \
             ELSE 'member' \
           END, \
           0, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(email)
    .bind(display_name)
    .bind(if forced_admin { 1i64 } else { 0i64 })
    .bind(now)
    .bind(now)
    .bind(subject)
    .bind(provider_id)
    .execute(&mut *tx)
    .await?;

    // Read the role back out for the audit log + the JWT mint in
    // the caller. Done inside the same transaction so the row we're
    // reading is the one we just inserted.
    let role: String = sqlx::query_scalar("SELECT role FROM users WHERE id = ?")
        .bind(&id)
        .fetch_one(&mut *tx)
        .await?;

    tx.commit().await?;

    // Audit row for the OIDC-driven user creation (audit Tier 1 #9).
    // Recorded outside the transaction since `audit::record` is
    // best-effort and we don't want a flake there to roll back the
    // user creation.
    audit::record(
        &state.pool,
        AuditEvent {
            actor_id: None,
            actor_email: "<oidc>",
            action: action::USER_CREATE,
            target_kind: Some("user"),
            target_id: Some(&id),
            details: Some(json!({
                "via": "oidc",
                "provider": provider_id,
                "email": email,
                "role": role,
            })),
        },
    )
    .await;

    tracing::info!(
        user_id = %id,
        provider = %provider_id,
        email = %email,
        role = %role,
        "oidc user created"
    );
    Ok(id)
}

/// Successful-auth landing page. JS attempts the snipdesk:// deep
/// link automatically; the token is also rendered into a visible
/// field with a Copy button so the user can paste manually if the OS
/// didn't claim the URL scheme. The page never reflects the token
/// back into the URL or DOM in a way an attacker could harvest -
/// content is bound to this single response.
fn render_callback_success(client_redirect: &str, snipdesk_token: &str, email: &str) -> Response {
    // Both the redirect and the token go into HTML attributes; escape
    // them for HTML attribute context to be safe even though we
    // control both values.
    let redirect_url = format!(
        "{client_redirect}?token={}",
        urlencoding::encode(snipdesk_token)
    );
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Signed in - SnipDesk</title>
  <style>
    body {{ font-family: -apple-system, "Segoe UI", system-ui, sans-serif; background: #0b0d12; color: #e5e7eb; padding: 40px; line-height: 1.5; }}
    .card {{ max-width: 540px; margin: 60px auto; background: #14171e; border: 1px solid #262a33; border-radius: 8px; padding: 28px; }}
    h1 {{ font-size: 18px; margin: 0 0 12px; }}
    .muted {{ color: #9ca3af; font-size: 13px; }}
    .actions {{ margin-top: 18px; display: flex; gap: 8px; flex-wrap: wrap; }}
    .btn {{ background: #6f8cff; color: #0b0d12; border: none; border-radius: 4px; padding: 8px 14px; cursor: pointer; font: inherit; }}
    .btn.secondary {{ background: transparent; color: #e5e7eb; border: 1px solid #262a33; }}
    .fallback {{ margin-top: 22px; padding-top: 18px; border-top: 1px solid #262a33; }}
    .token-row {{ display: flex; gap: 6px; margin-top: 8px; }}
    .token-row input {{ flex: 1; padding: 8px 10px; background: #0b0d12; color: #e5e7eb; border: 1px solid #262a33; border-radius: 4px; font-family: ui-monospace, Menlo, Consolas, monospace; font-size: 12px; }}
    .copied {{ color: #34d399; font-size: 12px; margin-left: 8px; }}
  </style>
</head>
<body>
  <div class="card" id="card">
    <h1>Signed in as {email_safe}</h1>
    <p class="muted" id="status">Returning to SnipDesk...</p>
    <div class="actions">
      <button class="btn" id="openBtn" type="button">Open SnipDesk</button>
      <button class="btn secondary" id="closeBtn" type="button">Close this tab</button>
    </div>
    <div class="fallback">
      <p class="muted">If SnipDesk didn't open automatically, copy this token and paste it into the desktop app's "Paste sign-in token" field:</p>
      <div class="token-row">
        <input type="text" id="tkn" readonly value="{token_safe}" />
        <button class="btn" onclick="navigator.clipboard.writeText(document.getElementById('tkn').value); document.getElementById('copied').textContent = 'Copied'">Copy</button>
        <span id="copied" class="copied"></span>
      </div>
    </div>
  </div>
  <script>
    var deepLink = "{redirect_safe}";
    function fireDeepLink() {{
      // Setting window.location instead of clicking an anchor: when
      // the OS picks up the URL scheme, browsers don't navigate the
      // tab elsewhere, which keeps the close-tab attempt below
      // operating on the same window context.
      window.location = deepLink;
    }}
    function attemptClose() {{
      // window.close() only succeeds on tabs the script itself opened
      // (window.open). For tabs the user navigated to - including
      // OAuth callbacks - most browsers silently ignore it. We try
      // anyway, then fall back to blanking the page so the visible
      // result is "tab can clearly be closed" instead of "tab still
      // shows my token".
      try {{ window.close(); }} catch (_e) {{}}
      setTimeout(function () {{
        if (!document.hidden && document.body) {{
          document.getElementById("status").textContent =
            "All set - you can close this tab.";
          document.getElementById("openBtn").style.display = "none";
          document.getElementById("closeBtn").textContent = "Close tab";
          // Strip the token from the page so it isn't sitting around
          // in browser history / paste buffer if the user wandered
          // off and the tab survived.
          var tknEl = document.getElementById("tkn");
          if (tknEl) tknEl.value = "(used)";
        }}
      }}, 500);
    }}
    document.getElementById("openBtn").addEventListener("click", fireDeepLink);
    document.getElementById("closeBtn").addEventListener("click", attemptClose);
    // Auto-fire the deep link and try to close shortly after. The OS
    // handoff lands SnipDesk in the foreground; the close attempt
    // here at least clears the auth tab in the browsers that allow
    // it (Chrome since v110 for URL-scheme-triggered windows), and
    // shows a clear "you can close this" message otherwise.
    fireDeepLink();
    setTimeout(attemptClose, 1500);
  </script>
</body>
</html>"#,
        email_safe = html_attr(email),
        redirect_safe = html_attr(&redirect_url),
        token_safe = html_attr(snipdesk_token),
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

fn render_callback_error(msg: &str) -> Response {
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Sign-in failed - SnipDesk</title>
  <style>
    body {{ font-family: -apple-system, "Segoe UI", system-ui, sans-serif; background: #0b0d12; color: #e5e7eb; padding: 40px; line-height: 1.5; }}
    .card {{ max-width: 540px; margin: 60px auto; background: #14171e; border: 1px solid #262a33; border-radius: 8px; padding: 28px; }}
    h1 {{ font-size: 18px; margin: 0 0 12px; }}
    .muted {{ color: #9ca3af; font-size: 13px; }}
  </style>
</head>
<body>
  <div class="card">
    <h1>Sign-in failed</h1>
    <p class="muted">{msg_safe}</p>
    <p class="muted">Close this tab and try again from SnipDesk.</p>
  </div>
</body>
</html>"#,
        msg_safe = html_attr(msg),
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

/// Read a top-level string claim straight off a verified JWT's
/// payload segment. openidconnect verified the signature already, so
/// the payload bytes can be trusted here; we just need fields the
/// typed claims API doesn't surface without a custom Client type.
fn read_string_claim(id_token_jwt: &str, key: &str) -> Option<String> {
    decode_jwt_payload(id_token_jwt)?
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Pull Keycloak's `realm_access.roles` out of a verified JWT.
/// Returns an empty vec when the claim is absent or malformed; the
/// caller treats absence as "no roles" rather than an error.
fn realm_roles_from_jwt(id_token_jwt: &str) -> Vec<String> {
    let Some(payload) = decode_jwt_payload(id_token_jwt) else {
        return Vec::new();
    };
    let Some(arr) = payload
        .get("realm_access")
        .and_then(|v| v.get("roles"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect()
}

/// Base64url-decode and JSON-parse the middle segment of a JWT.
/// Used by the small set of claim readers above; pulled out as a
/// helper so the segment-split logic only lives once.
fn decode_jwt_payload(id_token_jwt: &str) -> Option<serde_json::Value> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64URL, Engine as _};
    let parts: Vec<&str> = id_token_jwt.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload_bytes = B64URL.decode(parts[1]).ok()?;
    serde_json::from_slice(&payload_bytes).ok()
}

fn html_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn redirect_with_listed_scheme_passes_through() {
        let r = resolve_client_redirect(Some("snipdesk://auth"), &allow(&["snipdesk"]));
        assert_eq!(r, "snipdesk://auth");
    }

    #[test]
    fn redirect_with_whitelabel_scheme_passes_when_listed() {
        let r = resolve_client_redirect(Some("acme://auth"), &allow(&["snipdesk", "acme"]));
        assert_eq!(r, "acme://auth");
    }

    #[test]
    fn unknown_scheme_falls_back_to_first_listed() {
        let r = resolve_client_redirect(Some("evil://attack"), &allow(&["snipdesk", "acme"]));
        assert_eq!(r, "snipdesk://auth");
    }

    #[test]
    fn missing_redirect_falls_back_to_first_listed() {
        let r = resolve_client_redirect(None, &allow(&["acme", "snipdesk"]));
        assert_eq!(r, "acme://auth");
    }

    #[test]
    fn empty_allowlist_still_returns_snipdesk_default() {
        // Shouldn't reach this path with the config default in place,
        // but guard against it so an operator misconfig can't produce
        // a "://auth" string with no scheme.
        let r = resolve_client_redirect(Some("anything://x"), &[]);
        assert_eq!(r, "snipdesk://auth");
    }

    #[test]
    fn provider_id_round_trips() {
        for p in [Provider::Google, Provider::Keycloak] {
            assert_eq!(Provider::from_id(p.id()), Some(p));
        }
        assert_eq!(Provider::from_id("nope"), None);
    }

    #[test]
    fn realm_roles_parser_handles_empty_token() {
        assert!(realm_roles_from_jwt("not.a.jwt").is_empty());
        assert!(realm_roles_from_jwt("").is_empty());
    }
}
