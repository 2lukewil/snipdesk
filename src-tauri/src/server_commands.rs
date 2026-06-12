//! Teams-only IPC commands that talk to the snipdesk-server backend.
//! Auth (signup/login/logout), status reporting, manual sync trigger,
//! and the first-login migration of existing local snippets.
//!
//! Every command here is gated by `#[cfg(feature = "teams")]` in
//! `lib.rs::generate_handler!`; the Lite build doesn't include any of
//! this. The JWT is stored in the OS keychain (via
//! `snipdesk_teams::credentials`) so signing out wipes it cleanly and
//! the token never lives on disk in app-data.

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use snipdesk_teams::api::{self, ApiError, UserDto};
use snipdesk_teams::credentials;
use snipdesk_teams::sync::{self, SyncOutcome};

use crate::commands::CmdResult;
use crate::settings::SettingsPath;
use crate::AppState;

fn map_api_err(e: ApiError) -> String {
    e.to_string()
}

/// Custom URL scheme this build registers with the OS for the OIDC
/// callback deep link (and any future "snipdesk://open-snippet/..."
/// patterns). The literal "snipdesk" gets text-substituted at build
/// time by scripts/brand.mjs whenever the bundle sets
/// `deep_link_scheme` to something else, so a customer build that
/// uses `acme://` consistently sends its own scheme to the server
/// and accepts callbacks on it. Server-side, the matching
/// `[oidc].allowed_deep_link_schemes` config keeps the allowlist
/// from rejecting the non-default scheme.
pub const DEEP_LINK_SCHEME: &str = "snipdesk";

#[derive(Debug, Deserialize)]
pub struct SignupArgs {
    pub server_url: String,
    pub email: String,
    pub password: String,
    pub display_name: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginArgs {
    pub server_url: String,
    pub email: String,
    pub password: String,
}

/// Status snapshot for the settings UI. Returned synchronously from
/// `server_status` so the UI can render its signed-in / signed-out
/// state on first paint.
#[derive(Debug, Default, Clone, Serialize)]
pub struct ServerStatus {
    pub server_url: String,
    pub user: Option<UserDto>,
    pub signed_in: bool,
    pub last_sync: Option<SyncOutcome>,
    pub last_error: Option<String>,
}

// --- Sync-state helpers (stored in the same KV the engine uses) ---

fn save_signed_in_user(state: &AppState, user: &UserDto) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let json = serde_json::to_string(user).map_err(|e| e.to_string())?;
    db.save_sync_state("signed_in_user_json", &json)
        .map_err(|e| e.to_string())
}

fn load_signed_in_user(state: &AppState) -> Option<UserDto> {
    let db = state.db.lock().ok()?;
    let raw = db.load_sync_state("signed_in_user_json").ok()??;
    serde_json::from_str(&raw).ok()
}

fn clear_signed_in_user(state: &AppState) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    db.clear_sync_state("signed_in_user_json")
        .map_err(|e| e.to_string())
}

fn save_last_sync(state: &AppState, outcome: &SyncOutcome) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let json = serde_json::to_string(outcome).map_err(|e| e.to_string())?;
    db.save_sync_state("last_sync_outcome_json", &json)
        .map_err(|e| e.to_string())
}

fn load_last_sync(state: &AppState) -> Option<SyncOutcome> {
    let db = state.db.lock().ok()?;
    let raw = db.load_sync_state("last_sync_outcome_json").ok()??;
    serde_json::from_str(&raw).ok()
}

fn current_server_url(state: &AppState) -> String {
    state
        .settings
        .lock()
        .map(|s| s.server_url.clone())
        .unwrap_or_default()
}

/// Update both in-memory settings and the on-disk settings.json. Used
/// by signup/login so a successful sign-in survives a restart even if
/// the user didn't open the Settings modal to "Save" afterwards.
fn persist_server_url(app: &AppHandle, url: &str) -> Result<(), String> {
    let state = app.state::<AppState>();
    let new_settings = {
        let mut s = state.settings.lock().map_err(|e| e.to_string())?;
        s.server_url = url.to_string();
        s.clone()
    };
    let path = app.state::<SettingsPath>().0.clone();
    new_settings.save(&path).map_err(|e| e.to_string())
}

// --- Commands ---

/// Ask the server which sign-in surfaces it's configured for. The
/// desktop renders password fields + provider buttons strictly off
/// this response - no local guessing about which providers are
/// reachable. Unauthenticated server-side, so the caller doesn't
/// need credentials yet.
#[tauri::command]
pub async fn server_auth_methods(server_url: String) -> CmdResult<api::AuthMethodsResponse> {
    run_blocking(move || {
        let trimmed = server_url.trim().trim_end_matches('/').to_string();
        if trimmed.is_empty() {
            return Err("server URL is empty".to_string());
        }
        api::auth_methods(&trimmed).map_err(map_api_err)
    })
    .await
}

#[tauri::command]
pub async fn server_signup(app: AppHandle, args: SignupArgs) -> CmdResult<UserDto> {
    run_blocking(move || server_signup_blocking(app, args)).await
}

fn server_signup_blocking(app: AppHandle, args: SignupArgs) -> CmdResult<UserDto> {
    let state = app.state::<AppState>();
    let auth = api::signup(
        &args.server_url,
        &args.email,
        &args.password,
        &args.display_name,
    )
    .map_err(map_api_err)?;
    credentials::store(&args.server_url, &auth.token).map_err(|e| e.to_string())?;
    save_signed_in_user(&state, &auth.user)?;
    persist_server_url(&app, &args.server_url)?;
    Ok(auth.user)
}

#[tauri::command]
pub async fn server_login(app: AppHandle, args: LoginArgs) -> CmdResult<UserDto> {
    run_blocking(move || server_login_blocking(app, args)).await
}

fn server_login_blocking(app: AppHandle, args: LoginArgs) -> CmdResult<UserDto> {
    let state = app.state::<AppState>();
    let auth = api::login(&args.server_url, &args.email, &args.password).map_err(map_api_err)?;
    credentials::store(&args.server_url, &auth.token).map_err(|e| e.to_string())?;
    save_signed_in_user(&state, &auth.user)?;
    persist_server_url(&app, &args.server_url)?;
    Ok(auth.user)
}

#[tauri::command]
pub async fn server_logout(app: AppHandle) -> CmdResult<()> {
    // Off-thread: credential stores can stall (locked keyring, AV).
    run_blocking(move || server_logout_blocking(app)).await
}

fn server_logout_blocking(app: AppHandle) -> CmdResult<()> {
    let state = app.state::<AppState>();
    let server_url = current_server_url(&state);
    if !server_url.is_empty() {
        let _ = credentials::delete(&server_url);
    }
    clear_signed_in_user(&state)?;
    if let Ok(db) = state.db.lock() {
        // reset_sync_metadata drops team_snippets too, so the badge
        // ought to be zeroed on the same tick. Emit the update so the
        // sidebar redraws without waiting for the next pass.
        let _ = db.reset_sync_metadata();
    }
    refresh_team_snippet_count(&app, &state);
    Ok(())
}

/// Locked deployment defaults. `server_url` resolves runtime-managed
/// sources first (see `settings::managed_server_url`), then the
/// baked build-time value. Non-empty collapses the "Server URL"
/// inputs in the frontend; `""` = unmanaged build.
#[derive(Debug, Clone, Serialize)]
pub struct BrandDefaults {
    pub server_url: String,
    pub sso_only: bool,
}

#[tauri::command]
pub fn brand_defaults() -> BrandDefaults {
    let defaults = snipdesk_core::settings::Settings::default();
    BrandDefaults {
        server_url: snipdesk_core::settings::managed_server_url().unwrap_or(defaults.server_url),
        sso_only: defaults.prefer_sso_signin,
    }
}

#[tauri::command]
pub async fn server_status(app: AppHandle) -> CmdResult<ServerStatus> {
    // No network, but credentials::load hits the OS keychain and this
    // is polled constantly.
    run_blocking(move || server_status_blocking(app)).await
}

fn server_status_blocking(app: AppHandle) -> CmdResult<ServerStatus> {
    let state = app.state::<AppState>();
    let server_url = current_server_url(&state);
    let signed_in = if server_url.is_empty() {
        false
    } else {
        credentials::load(&server_url)
            .map(|t| t.is_some())
            .unwrap_or(false)
    };
    Ok(ServerStatus {
        server_url,
        user: if signed_in {
            load_signed_in_user(&state)
        } else {
            None
        },
        signed_in,
        last_sync: load_last_sync(&state),
        last_error: state.server_sync_error.lock().ok().and_then(|g| g.clone()),
    })
}

/// Record the outcome of any sync attempt (background tick, manual
/// Sync now, post-mutation kick) so the footer glyph reflects
/// reality. Failures emit an event so the frontend repaints without
/// waiting for the next poll; successes clear silently (the normal
/// server-sync event already repaints).
fn record_sync_failure(app: &AppHandle, state: &AppState, err: &str) {
    if let Ok(mut g) = state.server_sync_error.lock() {
        *g = Some(err.to_string());
    }
    let _ = app.emit("snipdesk://server-sync-failed", err.to_string());
}

fn record_sync_success(state: &AppState) {
    if let Ok(mut g) = state.server_sync_error.lock() {
        *g = None;
    }
}

/// Run blocking work (network, keychain) off the main thread. Sync
/// `#[tauri::command]` fns execute ON the main thread in Tauri 2, so
/// a hanging request inside one freezes the whole window for the
/// duration (the client visibly lags whenever the server is down).
/// Every network-touching command goes through here.
async fn run_blocking<T: Send + 'static>(
    f: impl FnOnce() -> CmdResult<T> + Send + 'static,
) -> CmdResult<T> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("background task failed: {e}"))?
}

#[tauri::command]
pub async fn server_sync_now(app: AppHandle) -> CmdResult<SyncOutcome> {
    run_blocking(move || server_sync_now_blocking(app)).await
}

fn server_sync_now_blocking(app: AppHandle) -> CmdResult<SyncOutcome> {
    let state = app.state::<AppState>();
    let server_url = current_server_url(&state);
    if server_url.is_empty() {
        return Err("no server configured".to_string());
    }
    let token = credentials::load(&server_url)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not signed in".to_string())?;
    let outcome = match sync::tick(&state.db, &server_url, &token) {
        Ok(o) => o,
        Err(ApiError::AccountInactive(msg)) => {
            handle_account_inactive(&app, &state, &server_url, &msg);
            return Err(msg);
        }
        Err(e) => {
            // Manual / kicked syncs share the failure ledger with the
            // background loop so the footer glyph turns regardless of
            // which path noticed the outage.
            let msg = map_api_err(e);
            record_sync_failure(&app, &state, &msg);
            return Err(msg);
        }
    };
    record_sync_success(&state);
    save_last_sync(&state, &outcome)?;
    refresh_team_snippet_count(&app, &state);
    // Persist the refreshed user record so server_status sees the new
    // role / display name without waiting for a reload.
    if let Some(u) = &outcome.user {
        let _ = save_signed_in_user(&state, u);
    }
    // Auto-rotated token: stash the new value in the keychain so the
    // next tick (and the next launch) uses it. The current request
    // already finished against the old token; we're swapping the
    // stored credential ahead of the next call.
    if let Some(new_token) = &outcome.refreshed_token {
        if let Err(e) = credentials::store(&server_url, new_token) {
            // The server already rotated; an unstored token means the
            // next sync runs on a credential the server may reject.
            record_sync_failure(
                &app,
                &state,
                &format!("couldn't store the refreshed session token: {e}"),
            );
        }
    }
    let _ = app.emit("snipdesk://server-sync", outcome.clone());
    Ok(outcome)
}

/// Server says the account is disabled or gone. Wipe local credentials
/// and emit a signed-out event so the frontend bounces the user back
/// to the login form with a clear reason. Idempotent - no-op if the
/// credential is already gone.
pub fn handle_account_inactive(app: &AppHandle, state: &AppState, server_url: &str, reason: &str) {
    let _ = credentials::delete(server_url);
    let _ = clear_signed_in_user(state);
    if let Ok(db) = state.db.lock() {
        let _ = db.reset_sync_metadata();
    }
    refresh_team_snippet_count(app, state);
    let _ = app.emit("snipdesk://server-account-inactive", reason.to_string());
    let _ = app.emit("snipdesk://server-signed-out", ());
}

/// Called from the deep-link handler in lib.rs when the OS hands us a
/// snipdesk:// URL. Pulls the token out of the query string, validates
/// it via /api/me (catches paste mistakes, expired returns, mismatch
/// against current server_url), persists to keychain, and emits the
/// usual server-sync event so the frontend redraws.
pub fn handle_oidc_deep_link(app: &AppHandle, url: &url::Url) -> Result<(), String> {
    // We only act on snipdesk://auth?token=... - any other path is
    // silently ignored (the scheme is reserved for this app, but a
    // future "snipdesk://open-snippet/abc" would land in this same
    // handler).
    if url.host_str() != Some("auth") {
        return Ok(());
    }
    let token = url
        .query_pairs()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
        .ok_or_else(|| "deep link missing token".to_string())?;
    if token.is_empty() {
        return Err("empty token in deep link".to_string());
    }
    // The deep-link event fires on the MAIN thread; the validation
    // round-trip below must not run there or a slow/dead server
    // freezes the window right as it comes to the foreground. The
    // window raise stays synchronous (it's why the user is looking
    // at the app); the network half moves to a worker thread.
    if let Some(win) = app.get_webview_window("main") {
        crate::show_and_focus(app, &win);
    }
    let app = app.clone();
    std::thread::spawn(move || {
        // Every bail-out emits signin-failed; otherwise the onboarding
        // panel sits on "Waiting for sign-in..." after the browser
        // already reported success.
        let fail = |msg: String| {
            eprintln!("deep link: {msg}");
            let _ = app.emit("snipdesk://signin-failed", msg);
        };
        let state = app.state::<AppState>();
        let server_url = current_server_url(&state);
        if server_url.is_empty() {
            fail("no server URL is configured, so the sign-in token couldn't be used".to_string());
            return;
        }
        // Validate against the server before persisting. If the user
        // somehow opened the deep link against the wrong server (or
        // the token was already revoked), the /api/me call errors
        // and we don't write garbage to the keychain.
        let me = match api::me(&server_url, &token) {
            Ok(me) => me,
            Err(e) => {
                fail(format!(
                    "the sign-in token didn't validate: {}",
                    map_api_err(e)
                ));
                return;
            }
        };
        if let Err(e) = credentials::store(&server_url, &token) {
            fail(format!(
                "couldn't save the sign-in to the credential store: {e}"
            ));
            return;
        }
        if let Err(e) = save_signed_in_user(&state, &me.user) {
            eprintln!("deep link: save user failed: {e}");
        }
        // Real sync, not just a notification: without it the team
        // library stays empty until the background loop's next tick,
        // which reads as "sign-in worked but nothing appeared".
        spawn_post_signin_sync(app.clone());
    });
    Ok(())
}

/// Run one sync on a background thread right after a sign-in, then
/// notify the UI. The OIDC paths (deep link + pasted token) land
/// here; the password path gets the same effect from the frontend's
/// afterSignedIn calling server_sync_now. Off-thread because the
/// deep-link callback runs on the main thread and a network
/// round-trip there would freeze the window.
fn spawn_post_signin_sync(app: AppHandle) {
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        let server_url = current_server_url(&state);
        let token = match credentials::load(&server_url) {
            Ok(Some(t)) => t,
            _ => return,
        };
        match sync::tick(&state.db, &server_url, &token) {
            Ok(outcome) => {
                let _ = save_last_sync(&state, &outcome);
            }
            Err(e) => eprintln!("post-sign-in sync failed: {e}"),
        }
        refresh_team_snippet_count(&app, &state);
        // Emitted AFTER the sync so the frontend's refresh sees the
        // freshly-pulled rows.
        let _ = app.emit("snipdesk://server-sync", ());
    });
}

/// Validate any credential stored for the current server URL against
/// the server, returning the live user or None. Unlike server_status
/// (purely local by design - it gates first-paint rendering), this
/// makes one /api/me round-trip, so callers reach for it only at
/// decision points: the onboarding sign-in panel uses it to tell a
/// REAL prior session ("already signed in as X on this device" -
/// keychain entries survive reinstalls) from a stale token, instead
/// of declaring "signed in" off mere keychain presence. A dead token
/// is wiped so the rest of the UI agrees; network errors return Err
/// and wipe nothing (offline must not sign anyone out).
#[tauri::command]
pub async fn server_validate_session(app: AppHandle) -> CmdResult<Option<UserDto>> {
    run_blocking(move || server_validate_session_blocking(app)).await
}

fn server_validate_session_blocking(app: AppHandle) -> CmdResult<Option<UserDto>> {
    let state = app.state::<AppState>();
    let server_url = current_server_url(&state);
    if server_url.is_empty() {
        return Ok(None);
    }
    let token = match credentials::load(&server_url) {
        Ok(Some(t)) => t,
        _ => return Ok(None),
    };
    match api::me(&server_url, &token) {
        Ok(me) => {
            save_signed_in_user(&state, &me.user)?;
            Ok(Some(me.user))
        }
        Err(ApiError::Unauthorized) => {
            // Token expired or revoked: clear it so server_status and
            // the sign-in surfaces stop claiming a session that the
            // server no longer honors.
            let _ = credentials::delete(&server_url);
            let _ = clear_signed_in_user(&state);
            refresh_team_snippet_count(&app, &state);
            Ok(None)
        }
        Err(ApiError::AccountInactive(reason)) => {
            handle_account_inactive(&app, &state, &server_url, &reason);
            Ok(None)
        }
        Err(e) => Err(map_api_err(e)),
    }
}

/// Build the URL the user should open in their browser to start the
/// OIDC dance. The desktop side opens this with `shell::open` (the
/// system browser), so the user gets their existing Google session.
/// The redirect param tells the server's callback to return us via
/// the `snipdesk://auth?token=...` deep link.
#[tauri::command]
pub fn server_oidc_start_url(
    app: AppHandle,
    server_url: String,
    start_path: Option<String>,
) -> CmdResult<String> {
    let server_url = server_url.trim().trim_end_matches('/').to_string();
    if server_url.is_empty() {
        return Err("server URL is required before signing in".to_string());
    }
    // Persist the URL up front - the deep-link handler reads it from
    // settings to know which server to associate the returned token
    // with. If we waited until after the browser dance, the handler
    // would have no idea which server's account it just signed in to.
    let state = app.state::<AppState>();
    persist_server_url(&app, &server_url)?;
    let _ = state;
    // `start_path` comes from the /api/auth/methods response (each
    // provider entry carries its own start_url). When the caller
    // doesn't supply one (older client builds, or a missing methods
    // fetch), fall back to the legacy unscoped Google route - the
    // server keeps that route mounted as a Google shim.
    let path = start_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.starts_with('/'))
        .unwrap_or("/api/auth/oidc/start");
    // Percent-encoded `<scheme>://auth` so it survives the query
    // string round-trip without the server having to decode the
    // colon. The scheme characters themselves are URL-safe (lower
    // ASCII letters + digits), so no escaping is needed on them.
    Ok(format!(
        "{server_url}{path}?redirect={DEEP_LINK_SCHEME}%3A%2F%2Fauth"
    ))
}

/// Manual fallback for the case where the deep link didn't fire (the
/// OS didn't claim the snipdesk:// scheme, an antivirus stripped it,
/// the user is on a corp-locked Windows config). The user copies the
/// token from the browser landing page and pastes it into the desktop
/// app; this command takes the pasted value, calls /api/me to confirm
/// it's valid, and persists it to the keychain just like the deep-
/// link path would have.
#[tauri::command]
pub async fn server_oidc_paste_token(app: AppHandle, token: String) -> CmdResult<UserDto> {
    run_blocking(move || server_oidc_paste_token_blocking(app, token)).await
}

fn server_oidc_paste_token_blocking(app: AppHandle, token: String) -> CmdResult<UserDto> {
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("token is empty".to_string());
    }
    let state = app.state::<AppState>();
    let server_url = current_server_url(&state);
    if server_url.is_empty() {
        return Err("set the server URL before pasting a token".to_string());
    }
    // Validate via /api/me - same probe used by every sync tick - so a
    // garbage paste returns a sensible error before we write to the
    // keychain.
    let me = api::me(&server_url, &token).map_err(map_api_err)?;
    credentials::store(&server_url, &token).map_err(|e| e.to_string())?;
    save_signed_in_user(&state, &me.user)?;
    // Same immediate sync the deep-link path runs; see
    // spawn_post_signin_sync for why a notification alone isn't
    // enough.
    spawn_post_signin_sync(app.clone());
    Ok(me.user)
}

/// List the server-side trash for the signed-in user. Returns the
/// `TrashView` shape verbatim from the server: id, version, payload,
/// created_at, deleted_at. The desktop renders these in a dedicated
/// trash panel without writing them to the local DB - they're
/// transient, and the source of truth is the server until restored.
#[tauri::command]
pub async fn server_trash_list(app: AppHandle) -> CmdResult<Vec<api::TrashView>> {
    run_blocking(move || server_trash_list_blocking(app)).await
}

fn server_trash_list_blocking(app: AppHandle) -> CmdResult<Vec<api::TrashView>> {
    let state = app.state::<AppState>();
    let server_url = current_server_url(&state);
    if server_url.is_empty() {
        return Err("no server configured".to_string());
    }
    let token = credentials::load(&server_url)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not signed in".to_string())?;
    api::list_trash(&server_url, &token).map_err(map_api_err)
}

/// Restore a snippet from the server-side trash. Server bumps the
/// version + clears is_deleted; the next sync tick will pull the row
/// back into the local snippets table via the normal upsert path, so
/// we just kick off `server_sync_now` after a successful restore.
#[tauri::command]
pub async fn server_trash_restore(app: AppHandle, id: String) -> CmdResult<()> {
    run_blocking(move || server_trash_restore_blocking(app, id)).await
}

fn server_trash_restore_blocking(app: AppHandle, id: String) -> CmdResult<()> {
    let state = app.state::<AppState>();
    let server_url = current_server_url(&state);
    if server_url.is_empty() {
        return Err("no server configured".to_string());
    }
    let token = credentials::load(&server_url)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not signed in".to_string())?;
    api::restore_snippet(&server_url, &token, &id).map_err(map_api_err)?;
    // Trigger an immediate pull so the restored row appears locally
    // without waiting for the next background tick.
    let outcome = sync::tick(&state.db, &server_url, &token).map_err(map_api_err)?;
    save_last_sync(&state, &outcome)?;
    if let Some(u) = &outcome.user {
        let _ = save_signed_in_user(&state, u);
    }
    let _ = app.emit("snipdesk://server-sync", outcome);
    Ok(())
}

/// Re-read the row count of team_snippets and update the
/// `team_snippet_count` atomic + emit `snipdesk://team-library-updated`.
/// Called after every server sync tick (manual or background) because
/// the library pull mutates that table.
pub fn refresh_team_snippet_count(app: &AppHandle, state: &AppState) {
    let count = state
        .db
        .lock()
        .ok()
        .and_then(|db| db.count_team_snippets().ok())
        .unwrap_or(0) as usize;
    state
        .team_snippet_count
        .store(count, std::sync::atomic::Ordering::SeqCst);
    let _ = app.emit("snipdesk://team-library-updated", ());
}

/// Args for `server_update_profile`. Three-state per field:
///   - field missing from JSON   → leave the server's value alone
///   - field present with value  → set to that value
///   - field present and null    → clear back to the server's [stats] default
///
/// JS callers idiomatically pass `undefined` for "leave alone" (which
/// serde-tauri converts to a missing field) and `null` for "clear".
/// The double-Option mirrors the wire body so the round-trip is
/// exact.
#[derive(Debug, Default, Deserialize)]
pub struct UpdateProfileArgs {
    #[serde(default, deserialize_with = "double_option")]
    pub wpm: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub hourly_wage: Option<Option<f64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub currency: Option<Option<String>>,
}

/// `Some(None)` if the JSON field is `null`, `Some(Some(v))` if it
/// has a value, never `None` when called - serde uses `default` for
/// the absent case.
fn double_option<'de, T, D>(d: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(d).map(Some)
}

/// PATCH the signed-in user's wpm/wage/currency on the server. On
/// success we re-save the local user record so `server_status`
/// reflects the new values immediately (the next /api/me probe would
/// pick them up too, but we don't want a 60s lag).
#[tauri::command]
pub async fn server_update_profile(app: AppHandle, args: UpdateProfileArgs) -> CmdResult<UserDto> {
    run_blocking(move || server_update_profile_blocking(app, args)).await
}

fn server_update_profile_blocking(app: AppHandle, args: UpdateProfileArgs) -> CmdResult<UserDto> {
    let state = app.state::<AppState>();
    let server_url = current_server_url(&state);
    if server_url.is_empty() {
        return Err("no server configured".to_string());
    }
    let token = credentials::load(&server_url)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not signed in".to_string())?;
    let body = api::UpdateMeBody {
        wpm: args.wpm,
        hourly_wage: args.hourly_wage,
        currency: args.currency,
    };
    let user = api::update_me(&server_url, &token, &body).map_err(map_api_err)?;
    save_signed_in_user(&state, &user)?;
    Ok(user)
}

#[tauri::command]
pub async fn server_migrate_local_snippets(app: AppHandle) -> CmdResult<usize> {
    run_blocking(move || server_migrate_local_snippets_blocking(app)).await
}

fn server_migrate_local_snippets_blocking(app: AppHandle) -> CmdResult<usize> {
    let state = app.state::<AppState>();
    let server_url = current_server_url(&state);
    if server_url.is_empty() {
        return Err("no server configured".to_string());
    }
    let token = credentials::load(&server_url)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not signed in".to_string())?;
    sync::migrate_existing_local(&state.db, &server_url, &token).map_err(map_api_err)
}

/// Spawn the background sync loop. Polls every
/// `team_library_sync_interval_mins` minutes (reusing the existing
/// cadence setting; phase 5 will rename it) and runs one `sync::tick`
/// per pass when the user is signed in. Emits `snipdesk://server-sync`
/// on each pass so the UI can refresh.
pub fn start_server_sync_thread(handle: AppHandle) {
    use std::thread;
    use std::time::{Duration, SystemTime};

    thread::spawn(move || {
        // Initial pause lets the UI paint before the first network call.
        thread::sleep(Duration::from_secs(8));
        let mut last_sync = SystemTime::UNIX_EPOCH;
        loop {
            let (server_url, interval_mins) = {
                let state = match handle.try_state::<AppState>() {
                    Some(s) => s,
                    None => return,
                };
                let g = match state.settings.lock() {
                    Ok(g) => g,
                    Err(_) => {
                        thread::sleep(Duration::from_secs(30));
                        continue;
                    }
                };
                (
                    g.server_url.clone(),
                    g.team_library_sync_interval_mins.max(1) as u64,
                )
            };
            if server_url.trim().is_empty() {
                thread::sleep(Duration::from_secs(30));
                continue;
            }
            let elapsed = SystemTime::now()
                .duration_since(last_sync)
                .unwrap_or(Duration::ZERO);
            if elapsed < Duration::from_secs(interval_mins * 60) {
                thread::sleep(Duration::from_secs(30));
                continue;
            }

            let token = match credentials::load(&server_url) {
                Ok(Some(t)) => t,
                _ => {
                    thread::sleep(Duration::from_secs(60));
                    continue;
                }
            };
            let state = match handle.try_state::<AppState>() {
                Some(s) => s,
                None => return,
            };

            match sync::tick(&state.db, &server_url, &token) {
                Ok(outcome) => {
                    record_sync_success(&state);
                    let _ = save_last_sync(&state, &outcome);
                    refresh_team_snippet_count(&handle, &state);
                    if let Some(u) = &outcome.user {
                        let _ = save_signed_in_user(&state, u);
                    }
                    if let Some(new_token) = &outcome.refreshed_token {
                        if let Err(e) = credentials::store(&server_url, new_token) {
                            // Same ledger as a failed tick: an unstored
                            // rotated token is a future sign-out.
                            record_sync_failure(
                                &handle,
                                &state,
                                &format!("couldn't store the refreshed session token: {e}"),
                            );
                        }
                    }
                    let _ = handle.emit("snipdesk://server-sync", outcome);
                }
                Err(ApiError::AccountInactive(msg)) => {
                    eprintln!("background sync: account inactive ({msg}); signing out");
                    handle_account_inactive(&handle, &state, &server_url, &msg);
                }
                Err(ApiError::Unauthorized) => {
                    // Don't wipe the credential on a possibly-transient
                    // 401; no refresh-token flow yet (v1.1), so a
                    // persistent 401 means a manual re-sign-in.
                    eprintln!("background sync got 401; leaving credential in place");
                    let _ = handle.emit("snipdesk://server-auth-warning", ());
                }
                Err(e) => {
                    // Surface the failure: the footer glyph reads
                    // ServerStatus.last_error, and the emitted event
                    // repaints it now instead of on the next poll.
                    eprintln!("background sync error: {e}");
                    record_sync_failure(&handle, &state, &map_api_err(e));
                }
            }
            last_sync = SystemTime::now();
        }
    });
}
