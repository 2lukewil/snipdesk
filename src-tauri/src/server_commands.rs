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
use tauri::{AppHandle, Emitter, Manager, State};

use snipdesk_teams::api::{self, ApiError, UserDto};
use snipdesk_teams::credentials;
use snipdesk_teams::sync::{self, SyncOutcome};

use crate::commands::CmdResult;
use crate::settings::SettingsPath;
use crate::AppState;

fn map_api_err(e: ApiError) -> String {
    e.to_string()
}

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

#[tauri::command]
pub fn server_signup(app: AppHandle, args: SignupArgs) -> CmdResult<UserDto> {
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
pub fn server_login(app: AppHandle, args: LoginArgs) -> CmdResult<UserDto> {
    let state = app.state::<AppState>();
    let auth = api::login(&args.server_url, &args.email, &args.password).map_err(map_api_err)?;
    credentials::store(&args.server_url, &auth.token).map_err(|e| e.to_string())?;
    save_signed_in_user(&state, &auth.user)?;
    persist_server_url(&app, &args.server_url)?;
    Ok(auth.user)
}

#[tauri::command]
pub fn server_logout(app: AppHandle) -> CmdResult<()> {
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

#[tauri::command]
pub fn server_status(state: State<'_, AppState>) -> CmdResult<ServerStatus> {
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
        last_error: None,
    })
}

#[tauri::command]
pub fn server_sync_now(app: AppHandle) -> CmdResult<SyncOutcome> {
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
        Err(e) => return Err(map_api_err(e)),
    };
    save_last_sync(&state, &outcome)?;
    refresh_team_snippet_count(&app, &state);
    // Persist the refreshed user record so server_status sees the new
    // role / display name without waiting for a reload.
    if let Some(u) = &outcome.user {
        let _ = save_signed_in_user(&state, u);
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

/// List the server-side trash for the signed-in user. Returns the
/// `TrashView` shape verbatim from the server: id, version, payload,
/// created_at, deleted_at. The desktop renders these in a dedicated
/// trash panel without writing them to the local DB - they're
/// transient, and the source of truth is the server until restored.
#[tauri::command]
pub fn server_trash_list(app: AppHandle) -> CmdResult<Vec<api::TrashView>> {
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
pub fn server_trash_restore(app: AppHandle, id: String) -> CmdResult<()> {
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

#[tauri::command]
pub fn server_migrate_local_snippets(app: AppHandle) -> CmdResult<usize> {
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
                    let _ = save_last_sync(&state, &outcome);
                    refresh_team_snippet_count(&handle, &state);
                    if let Some(u) = &outcome.user {
                        let _ = save_signed_in_user(&state, u);
                    }
                    let _ = handle.emit("snipdesk://server-sync", outcome);
                }
                Err(ApiError::AccountInactive(msg)) => {
                    eprintln!("background sync: account inactive ({msg}); signing out");
                    handle_account_inactive(&handle, &state, &server_url, &msg);
                }
                Err(ApiError::Unauthorized) => {
                    // Earlier we auto-deleted the credential here. That
                    // was too aggressive - a transient 401 (or any
                    // misclassification on the server side) would wipe
                    // the user's session and confuse them. With no
                    // refresh-token flow yet (v1.1), we just log and
                    // let the user re-sign-in manually if it persists.
                    eprintln!("background sync got 401; leaving credential in place");
                    let _ = handle.emit("snipdesk://server-auth-warning", ());
                }
                Err(e) => {
                    eprintln!("background sync error: {e}");
                }
            }
            last_sync = SystemTime::now();
        }
    });
}
