//! Google Workspace OIDC sign-in.
//!
//! Two-step browser dance:
//!   1. `GET /api/auth/oidc/start` - the desktop client opens this in
//!      the user's browser. We generate a CSRF state, a PKCE verifier,
//!      and a nonce, stash all three keyed by state, then 302 the
//!      browser to Google's authorize endpoint.
//!   2. `GET /api/auth/oidc/callback` - Google redirects here after
//!      the user signs in. We look up the stored state, exchange the
//!      code for tokens, verify the ID token (signature against
//!      Google's JWKS, audience, issuer, nonce match, optional
//!      `hd` claim against required_hd), find-or-create the user, and
//!      issue our own HS256 JWT. The response is an HTML page that
//!      attempts a `snipdesk://auth?token=...` deep link AND exposes
//!      the token for manual copy as a fallback.
//!
//! State store: in-memory `HashMap<state, PendingAuth>` behind a
//! Mutex. Entries expire after 10 minutes; each request prunes
//! expired entries before doing its own work. For a v1 single-process
//! deployment this is fine. A multi-instance deploy would need a
//! shared store (Redis), but that's a v2 concern.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::Utc;
use once_cell::sync::Lazy;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::reqwest::async_http_client;
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::issue_token;
use crate::config::GoogleOidcConfig;
use crate::error::ApiError;
use crate::http::AppState;

/// A pending authorization waiting for its callback. The keys we
/// need to keep alive between /start and /callback: PKCE verifier
/// (proves the same user agent initiated both calls), nonce (binds
/// the ID token to this specific authorization), and the desktop
/// redirect URL (so we know where to send the user once we've
/// minted their JWT).
struct PendingAuth {
    pkce_verifier: PkceCodeVerifier,
    nonce: Nonce,
    /// The custom-scheme URL we'll redirect the browser to with the
    /// issued token appended. Currently always `snipdesk://auth`; the
    /// `?redirect` query param on /start lets the future support
    /// other client builds without code changes.
    client_redirect: String,
    created_at: i64,
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

/// Build the openidconnect Client for Google. Discovery hits
/// https://accounts.google.com/.well-known/openid-configuration once
/// per request (the crate doesn't cache); fine for our v1 traffic
/// shape. A multi-instance / high-RPS deploy would want a cached
/// metadata document.
async fn google_client(cfg: &GoogleOidcConfig) -> Result<CoreClient, ApiError> {
    let issuer = IssuerUrl::new("https://accounts.google.com".to_string())
        .map_err(|e| ApiError::internal(format!("oidc issuer url: {e}")))?;
    let metadata = CoreProviderMetadata::discover_async(issuer, async_http_client)
        .await
        .map_err(|e| ApiError::internal(format!("oidc discovery: {e}")))?;
    let redirect = RedirectUrl::new(cfg.redirect_uri.clone())
        .map_err(|e| ApiError::internal(format!("oidc redirect url: {e}")))?;
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(cfg.client_id.clone()),
        Some(ClientSecret::new(cfg.client_secret.clone())),
    )
    .set_redirect_uri(redirect);
    Ok(client)
}

#[derive(Debug, Deserialize)]
pub struct StartQuery {
    /// Where the callback should send the user after issuing a JWT.
    /// Defaults to `snipdesk://auth` (desktop deep link). Capped to
    /// the small allowlist below to keep this from becoming an open
    /// redirector.
    #[serde(default)]
    pub redirect: Option<String>,
}

/// Kick off the OIDC dance. Generates state + PKCE + nonce, stashes
/// them keyed by state, returns a 302 to Google's authorize endpoint.
pub async fn start(
    State(state_app): State<AppState>,
    Query(q): Query<StartQuery>,
) -> Result<Response, ApiError> {
    let cfg = google_cfg(&state_app)?;
    let now = Utc::now().timestamp();
    prune_pending(now);

    let client = google_client(&cfg).await?;

    // PKCE verifier + challenge. We send the challenge to Google in
    // the authorize step; the verifier comes back to us in the
    // callback's token exchange. Without this an attacker who
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

    let client_redirect = match q.redirect.as_deref() {
        // Allowlist: only known-safe schemes. Open-redirect prevention.
        Some(s) if s.starts_with("snipdesk://") => s.to_string(),
        _ => "snipdesk://auth".to_string(),
    };

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
                pkce_verifier,
                nonce,
                client_redirect,
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

/// Google redirects here after the user signs in (or errors out).
/// We exchange the code for an ID token, verify the token, find or
/// create the matching local user, and respond with an HTML page
/// that fires the desktop deep link plus a copy-paste fallback.
pub async fn callback(
    State(state_app): State<AppState>,
    Query(q): Query<CallbackQuery>,
) -> Result<Response, ApiError> {
    if let Some(err) = q.error.as_deref() {
        let desc = q.error_description.as_deref().unwrap_or("");
        return Ok(render_callback_error(&format!(
            "Google declined the sign-in: {err}{}{desc}",
            if desc.is_empty() { "" } else { " - " }
        )));
    }

    let code = q
        .code
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("missing_code", "no code in callback"))?;
    let state = q
        .state
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("missing_state", "no state in callback"))?;

    let now = Utc::now().timestamp();
    prune_pending(now);

    let pending = pending_store()
        .lock()
        .ok()
        .and_then(|mut s| s.remove(state))
        .ok_or_else(|| {
            ApiError::bad_request(
                "unknown_state",
                "this sign-in attempt has expired or was already used",
            )
        })?;

    let cfg = google_cfg(&state_app)?;
    let client = google_client(&cfg).await?;

    let token_response = client
        .exchange_code(AuthorizationCode::new(code.to_string()))
        .set_pkce_verifier(pending.pkce_verifier)
        .request_async(async_http_client)
        .await
        .map_err(|e| ApiError::bad_request("token_exchange", format!("token exchange: {e}")))?;

    let id_token = token_response
        .id_token()
        .ok_or_else(|| ApiError::internal("Google response missing id_token"))?;

    // Verify the ID token: signature via Google's JWKS, audience
    // matches our client_id, issuer is google, nonce matches the one
    // we generated.
    let id_token_verifier = client.id_token_verifier();
    let claims = id_token
        .claims(&id_token_verifier, &pending.nonce)
        .map_err(|e| ApiError::bad_request("id_token", format!("id_token verify: {e}")))?;

    // Workspace lockdown: `hd` claim must equal required_hd when set.
    // Google sets `hd` only for Workspace-managed accounts; personal
    // @gmail.com accounts have no hd. We read it from the raw JWT
    // payload because openidconnect's typed claims default to
    // EmptyAdditionalClaims - defining a custom CoreClient type just
    // to read one field is more boilerplate than it's worth. The
    // signature was already verified above, so the payload bytes are
    // trustworthy here.
    if let Some(required) = cfg.required_hd.as_deref() {
        let hd_claim = read_hd_claim(&id_token.to_string()).unwrap_or_default();
        if hd_claim != required {
            return Ok(render_callback_error(&format!(
                "This server is locked to the {required} Workspace. \
                 Sign in with a {required} account."
            )));
        }
    }

    let email = claims
        .email()
        .ok_or_else(|| ApiError::bad_request("no_email", "Google did not return an email"))?
        .as_str()
        .to_lowercase();

    if !cfg.allowed_email_domains.is_empty() {
        let domain = email.split('@').nth(1).unwrap_or("");
        if !cfg
            .allowed_email_domains
            .iter()
            .any(|d| d.eq_ignore_ascii_case(domain))
        {
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

    let google_sub = claims.subject().as_str().to_string();

    // Find-or-create. Match strategy:
    //   1. oidc_subject = google_sub -> existing OIDC user
    //   2. email = lookup -> existing password user; link by setting
    //      their oidc_subject (account-merging UX).
    //   3. otherwise create a fresh row.
    //
    // First-admin auto-promotion: a brand-new org gets its first
    // OIDC user promoted to admin, same as the password signup path.
    let user_id = upsert_oidc_user(&state_app, &google_sub, &email, &display_name).await?;

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

    Ok(render_callback_success(
        &pending.client_redirect,
        &snipdesk_token,
        &email,
    ))
}

/// Insert-or-update the user matching this Google sub. Returns the
/// resulting user's id.
async fn upsert_oidc_user(
    state: &AppState,
    google_sub: &str,
    email: &str,
    display_name: &str,
) -> Result<String, ApiError> {
    let mut tx = state.pool.begin().await?;

    // First: existing OIDC user.
    if let Some((id, is_disabled)) = sqlx::query_as::<_, (String, i64)>(
        "SELECT id, is_disabled FROM users WHERE oidc_subject = ?",
    )
    .bind(google_sub)
    .fetch_optional(&mut *tx)
    .await?
    {
        if is_disabled != 0 {
            return Err(ApiError::forbidden(
                "account_disabled",
                "your account is disabled - contact your administrator",
            ));
        }
        // Refresh display name in case it changed in Google.
        sqlx::query("UPDATE users SET display_name = ? WHERE id = ?")
            .bind(display_name)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(id);
    }

    // Second: existing password account with the same email - link.
    if let Some((id, is_disabled)) =
        sqlx::query_as::<_, (String, i64)>("SELECT id, is_disabled FROM users WHERE email = ?")
            .bind(email)
            .fetch_optional(&mut *tx)
            .await?
    {
        if is_disabled != 0 {
            return Err(ApiError::forbidden(
                "account_disabled",
                "your account is disabled - contact your administrator",
            ));
        }
        sqlx::query("UPDATE users SET oidc_subject = ?, display_name = ? WHERE id = ?")
            .bind(google_sub)
            .bind(display_name)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(id);
    }

    // Third: brand-new user.
    let admin_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE role = 'admin'")
        .fetch_one(&mut *tx)
        .await?;
    let role = if admin_count.0 == 0 {
        "admin"
    } else {
        "member"
    };
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO users (id, email, display_name, role, is_disabled, created_at, last_seen_at, oidc_subject) \
         VALUES (?, ?, ?, ?, 0, ?, ?, ?)",
    )
    .bind(&id)
    .bind(email)
    .bind(display_name)
    .bind(role)
    .bind(now)
    .bind(now)
    .bind(google_sub)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(user_id = %id, email = %email, role, "oidc user created");
    Ok(id)
}

fn google_cfg(state: &AppState) -> Result<GoogleOidcConfig, ApiError> {
    state.oidc_google.clone().ok_or_else(|| {
        ApiError::bad_request(
            "oidc_disabled",
            "this server doesn't have Google OIDC configured",
        )
    })
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

/// Read the `hd` (hosted domain) claim straight off the verified
/// JWT's payload segment. openidconnect verified the signature
/// already, so the payload bytes can be trusted here; we just need
/// the one field that the typed claims API doesn't surface without
/// a custom Client type.
fn read_hd_claim(id_token_jwt: &str) -> Option<String> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64URL, Engine as _};
    let parts: Vec<&str> = id_token_jwt.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload_bytes = B64URL.decode(parts[1]).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
    json.get("hd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn html_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
