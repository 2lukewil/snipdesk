// IPC surface - local to this crate because every fn is `#[tauri::command]`.
mod commands;
#[cfg(feature = "teams")]
mod server_commands;

// Re-export shared modules under short names so call sites stay stable as
// crates get reshuffled. Add new re-exports here when modules move.
pub use snipdesk_core::{backup, db, logging, paste, settings, shared_library};
// Teams-only - gated so the offline build's dep tree contains no `snipdesk-teams`
// and no `ureq`. Verify with `cargo tree --no-default-features`.
#[cfg(feature = "teams")]
pub use snipdesk_teams::shared_url;

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
#[cfg(feature = "teams")]
use std::sync::atomic::{AtomicI64, AtomicUsize};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
#[cfg(feature = "teams")]
use std::time::SystemTime;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _AutoManagerExt};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

pub struct AppState {
    pub db: Mutex<db::Db>,
    pub settings: Mutex<settings::Settings>,
    /// Set while a native file picker is up so the launcher doesn't blur-hide.
    pub hide_on_blur_suppressed: AtomicBool,
    /// Foreground HWND captured before SnipDesk steals focus. `use_snippet`
    /// restores to this before auto-typing. 0 = none captured.
    pub target_hwnd: AtomicIsize,
    /// Last-known minimized state. The Resized handler acts only on the
    /// not-minimized -> minimized transition; without this, the burst of
    /// Resized events Windows fires during restore animations (some of which
    /// momentarily report is_minimized=true) re-triggers minimize-to-tray
    /// and cycles the window open/closed.
    pub was_minimized: AtomicBool,
    /// Set while a Settings hotkey-capture field has focus. Global
    /// shortcuts stay registered with the OS but their handlers
    /// no-op, so typing a chord into the capture field can't also
    /// trigger the action it's currently bound to (the old behavior
    /// hid the window mid-capture when the user pressed the active
    /// hotkey).
    pub hotkeys_suspended: AtomicBool,
    /// Team-library sync status - three atomics rather than a Mutex<struct>
    /// because the frontend polls these on every status tick.
    #[cfg(feature = "teams")]
    pub team_last_fetched_unix: AtomicI64,
    #[cfg(feature = "teams")]
    pub team_snippet_count: AtomicUsize,
    #[cfg(feature = "teams")]
    pub team_last_error: Mutex<Option<String>>,
}

// capture_foreground_hwnd / restore_foreground now live in snipdesk_core::paste.

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();

    // Single-instance MUST be registered before any other plugin per
    // tauri-plugin-deep-link's docs. With the `deep-link` feature flag
    // enabled, the plugin's spawn-detection automatically forwards
    // snipdesk://... args from the second exe launch into the running
    // instance, so the deep-link plugin's on_open_url fires on the
    // signed-in session instead of a fresh process. The closure body
    // is a no-op for that reason - the forwarding happens before we
    // ever get here. Teams-only because the URL scheme is teams-only.
    #[cfg(feature = "teams")]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|_app, _argv, _cwd| {
        // intentionally empty - deep-link plugin handles the forwarding
    }));

    let builder = builder
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        // Auto-update (both flavors). `process` provides `relaunch()` after
        // the updater installs the new bundle. Endpoint + pubkey live in
        // tauri.conf.json; the frontend drives check/download/install.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        // Remember the main window's last size/position across launches.
        .plugin(tauri_plugin_window_state::Builder::default().build());

    // Teams build only: shell for opening the system browser at OIDC
    // start time, deep-link for receiving the snipdesk://auth?token=...
    // callback. on_open_url is wired in setup() below. The scheme is
    // also register_all()'d there so dev-mode (no NSIS installer) and
    // production installs both work; the call is idempotent.
    #[cfg(feature = "teams")]
    let builder = builder
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_deep_link::init());

    builder
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .expect("failed to resolve app data dir");
            std::fs::create_dir_all(&data_dir).ok();

            let db_path = data_dir.join("snippets.db");
            let settings_path = data_dir.join("settings.json");

            // Settings before logging/backup - both read retention windows from it.
            let settings = settings::Settings::load_or_default(&settings_path);

            // Deployment builds bake a default server URL (brand
            // bundle or SNIPDESK_DEFAULT_SERVER_URL at compile time).
            // The baked value is authoritative: if an update ships a
            // new URL, adopt it over whatever an older release
            // persisted, so the operator can move the fleet by
            // pushing a tag. Stock builds bake "" and never enter
            // this branch, so a self-managed user's hand-entered URL
            // is untouched.
            #[cfg(feature = "teams")]
            let settings = {
                let mut settings = settings;
                let baked = settings::Settings::default().server_url;
                if !baked.is_empty() && settings.server_url != baked {
                    settings.server_url = baked;
                    if let Err(e) = settings.save(&settings_path) {
                        eprintln!("failed to persist baked server URL: {e}");
                    }
                }
                settings
            };

            // Logging before Db::open so a corrupt-schema panic lands in snipdesk.log.
            logging::init(&data_dir, settings.log_retention_days);

            let db = db::Db::open(&db_path).expect("failed to open snippet db");

            // Expire old local-trash snapshots per the retention
            // setting (0 keeps forever). Startup-only sweep, same
            // pattern as backup retention below.
            if let Ok(purged) = db.purge_local_trash(settings.local_trash_retention_days) {
                if purged > 0 {
                    eprintln!("local trash: purged {purged} expired snapshot(s)");
                }
            }

            backup::init_schedule(&data_dir, &db_path, settings.backup_retention_days);

            app.manage(AppState {
                db: Mutex::new(db),
                settings: Mutex::new(settings.clone()),
                hide_on_blur_suppressed: AtomicBool::new(false),
                target_hwnd: AtomicIsize::new(0),
                was_minimized: AtomicBool::new(false),
                hotkeys_suspended: AtomicBool::new(false),
                #[cfg(feature = "teams")]
                team_last_fetched_unix: AtomicI64::new(0),
                #[cfg(feature = "teams")]
                team_snippet_count: AtomicUsize::new(0),
                #[cfg(feature = "teams")]
                team_last_error: Mutex::new(None),
            });
            app.manage(settings::SettingsPath(settings_path));

            // Teams-only - the free build has no network-touching threads.
            #[cfg(feature = "teams")]
            {
                start_team_sync_thread(app.handle().clone());
                // Server-backed personal-snippet sync. Independent loop
                // from the legacy team-library polling because the
                // cadences and auth model are different.
                server_commands::start_server_sync_thread(app.handle().clone());

                // Register the snipdesk:// URL scheme at runtime so
                // Windows/Linux know what to do when the browser
                // navigates to snipdesk://auth?token=. The NSIS
                // installer handles this for production builds; the
                // explicit register_all() also covers dev (`tauri
                // dev`) where no installer ran. Idempotent on macOS,
                // where the Info.plist association is already set.
                use tauri_plugin_deep_link::DeepLinkExt;
                if let Err(e) = app.deep_link().register_all() {
                    eprintln!("deep link scheme registration failed: {e}");
                }

                // Wire the snipdesk:// URL handler. The OAuth landing
                // page in the user's browser fires
                // `snipdesk://auth?token=<jwt>`; the OS hands it to
                // the running Tauri app; this closure extracts the
                // token, persists it via credentials::store under the
                // currently-configured server URL, then triggers a
                // sync so the UI updates without waiting for the
                // 5-minute background tick. Errors get logged but
                // don't crash - a malformed URL just produces a no-op.
                let handle_for_deeplink = app.handle().clone();
                app.deep_link().on_open_url(move |event| {
                    for url in event.urls() {
                        if let Err(e) =
                            server_commands::handle_oidc_deep_link(&handle_for_deeplink, &url)
                        {
                            eprintln!("oidc deep link handling failed: {e}");
                        }
                    }
                });
            }

            // --- System tray ---
            let open_accelerator = friendly_shortcut(&settings.hotkey);
            let open_item = MenuItem::with_id(
                app,
                "open",
                "Open SnipDesk",
                true,
                Some(open_accelerator.as_str()),
            )?;
            let new_item = MenuItem::with_id(app, "new", "New Snippet...", true, None::<&str>)?;
            let settings_item =
                MenuItem::with_id(app, "settings", "Settings...", true, None::<&str>)?;
            let sep1 = PredefinedMenuItem::separator(app)?;
            let sep2 = PredefinedMenuItem::separator(app)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit SnipDesk", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &open_item,
                    &sep1,
                    &new_item,
                    &settings_item,
                    &sep2,
                    &quit_item,
                ],
            )?;

            let mut tray_builder = TrayIconBuilder::with_id("main-tray")
                .tooltip(format!("SnipDesk - {open_accelerator}"))
                .menu(&menu)
                .show_menu_on_left_click(false);
            if let Some(icon) = app.default_window_icon() {
                tray_builder = tray_builder.icon(icon.clone());
            }
            let _tray = tray_builder
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => {
                        if let Some(win) = app.get_webview_window("main") {
                            show_and_focus(app, &win);
                            let _ = win.emit("snipdesk://opened", ());
                        }
                    }
                    "new" => {
                        if let Some(win) = app.get_webview_window("main") {
                            show_and_focus(app, &win);
                            let _ = win.emit("snipdesk://open-editor", ());
                        }
                    }
                    "settings" => {
                        if let Some(win) = app.get_webview_window("main") {
                            show_and_focus(app, &win);
                            let _ = win.emit("snipdesk://open-settings", ());
                        }
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let handle = tray.app_handle();
                        if let Some(win) = handle.get_webview_window("main") {
                            toggle_window_with_state(handle, &win);
                        }
                    }
                })
                .build(app)?;

            // --- Global hotkey ---
            let shortcut = parse_shortcut(&settings.hotkey).unwrap_or_else(|| {
                // Must mirror Settings::default() (Alt+Space).
                Shortcut::new(Some(Modifiers::ALT), Code::Space)
            });

            let app_handle = app.handle().clone();
            app.global_shortcut()
                .on_shortcut(shortcut, move |_app, _sc, event| {
                    if hotkeys_are_suspended(&app_handle) {
                        return;
                    }
                    if event.state() == ShortcutState::Pressed {
                        if let Some(win) = app_handle.get_webview_window("main") {
                            toggle_window_with_state(&app_handle, &win);
                        }
                    }
                })?;

            // Quick-add hotkey. Empty = disabled; malformed = log + skip
            // (a typo here must not brick launch).
            if !settings.quick_add_hotkey.trim().is_empty() {
                if let Some(quick_sc) = parse_shortcut(&settings.quick_add_hotkey) {
                    let quick_handle = app.handle().clone();
                    if let Err(err) =
                        app.global_shortcut()
                            .on_shortcut(quick_sc, move |_app, _sc, event| {
                                if hotkeys_are_suspended(&quick_handle) {
                                    return;
                                }
                                if event.state() == ShortcutState::Pressed {
                                    trigger_quick_add_from_selection(&quick_handle);
                                }
                            })
                    {
                        logging::log_error(&format!("quick-add hotkey register failed: {err}"));
                    }
                } else {
                    logging::log_error(&format!(
                        "quick-add hotkey not recognized: {}",
                        settings.quick_add_hotkey
                    ));
                }
            }

            // Windows fires Focused/Resized in unreliable orders during
            // minimize/restore + taskbar clicks. We act on settled state
            // (post-delay) or explicit transitions, never on a single event.
            if let Some(win) = app.get_webview_window("main") {
                let win_outer = win.clone();
                let handle = app.handle().clone();
                win.on_window_event(move |event| match event {
                    // X / Alt+F4 / task manager close.
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        let close_to_tray = handle
                            .try_state::<AppState>()
                            .and_then(|s| s.settings.lock().ok().map(|g| g.close_to_tray))
                            .unwrap_or(true);
                        if close_to_tray {
                            api.prevent_close();
                            let _ = win_outer.hide();
                        }
                    }

                    // Act only on not-minimized -> minimized transitions.
                    // The naive "if is_minimized, hide" path mis-fires during
                    // restore animations where Windows briefly reports
                    // is_minimized=true mid-restore.
                    tauri::WindowEvent::Resized(_) => {
                        let currently_minimized = win_outer.is_minimized().unwrap_or(false);
                        let prev_minimized = handle
                            .try_state::<AppState>()
                            .map(|s| s.was_minimized.swap(currently_minimized, Ordering::SeqCst))
                            .unwrap_or(false);
                        let just_minimized = currently_minimized && !prev_minimized;
                        if just_minimized {
                            let minimize_to_tray = handle
                                .try_state::<AppState>()
                                .and_then(|s| s.settings.lock().ok().map(|g| g.minimize_to_tray))
                                .unwrap_or(false);
                            if minimize_to_tray {
                                // Hide only - unminimizing here interrupts
                                // the minimize animation and flashes the
                                // full-size window for a frame. The unminimize
                                // happens inside toggle_window_with_state
                                // while still hidden.
                                let _ = win_outer.hide();
                            }
                        }
                    }

                    // Focused(false) also fires during minimize, close-to-tray,
                    // tray-menu activation, and taskbar-restore focus routing.
                    // Wait for state to settle, then hide only if all four
                    // hold: visible, not minimized, still unfocused, not
                    // suppressed (no file dialog up).
                    tauri::WindowEvent::Focused(false) => {
                        // Opt-in via `hide_on_blur` (off by default).
                        let (hide_on_blur, suppressed) = handle
                            .try_state::<AppState>()
                            .and_then(|s| {
                                s.settings.lock().ok().map(|g| {
                                    (
                                        g.hide_on_blur,
                                        s.hide_on_blur_suppressed.load(Ordering::SeqCst),
                                    )
                                })
                            })
                            .unwrap_or((false, false));
                        if !hide_on_blur || suppressed {
                            return;
                        }
                        let handle_inner = handle.clone();
                        let win_inner = win_outer.clone();
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(200));
                            // File dialog may have opened during the settle window.
                            let suppressed = handle_inner
                                .try_state::<AppState>()
                                .map(|s| s.hide_on_blur_suppressed.load(Ordering::SeqCst))
                                .unwrap_or(false);
                            if suppressed {
                                return;
                            }
                            let is_minimized = win_inner.is_minimized().unwrap_or(false);
                            let is_visible = win_inner.is_visible().unwrap_or(false);
                            let is_focused = win_inner.is_focused().unwrap_or(false);
                            if is_visible && !is_minimized && !is_focused {
                                let _ = win_inner.hide();
                            }
                        });
                    }

                    _ => {}
                });
            }

            // --- Autostart + first-run launch state ---
            let launched_with_autostart_flag = std::env::args().any(|a| a == "--autostart");
            if let Err(err) = apply_autostart(app.handle(), settings.start_with_windows) {
                eprintln!("autostart sync failed: {err}");
            }

            if let Some(win) = app.get_webview_window("main") {
                let _ = win.set_always_on_top(settings.always_on_top);
            }

            // Launch visibility, in priority order:
            //   --autostart flag    -> stay hidden (OS login launch)
            //   start_in_tray       -> stay hidden
            //   onboarding pending  -> show + first-run hint
            //   default             -> show
            //
            // tauri.conf.json keeps `visible: false` to avoid a blank-white
            // frame during webview init. Show happens here, post-paint.
            if let Some(win) = app.get_webview_window("main") {
                let start_hidden = launched_with_autostart_flag || settings.start_in_tray;
                if start_hidden {
                    // Config already hides it.
                } else if !settings.onboarding_completed {
                    let _ = win.show();
                    let _ = win.set_focus();
                    let _ = win.emit("snipdesk://first-run", ());
                } else {
                    let _ = win.show();
                    let _ = win.set_focus();
                    let _ = win.emit("snipdesk://opened", ());
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_snippets,
            commands::get_snippet,
            commands::create_snippet,
            commands::update_snippet,
            commands::delete_snippet,
            commands::duplicate_snippet,
            commands::list_tags,
            commands::list_folders,
            commands::create_folder,
            commands::rename_folder,
            commands::delete_folder,
            commands::get_var_history,
            commands::use_snippet,
            commands::get_settings,
            commands::set_hotkey_capture,
            commands::update_settings,
            commands::export_snippets,
            commands::import_snippets,
            commands::parse_snippet_file,
            commands::import_snippet_items,
            commands::local_trash_list,
            commands::local_trash_restore,
            commands::local_trash_delete,
            commands::hide_window,
            commands::suspend_hide_on_blur,
            commands::resume_hide_on_blur,
            commands::check_title_conflict,
            // cfg works inside generate_handler!; in the free build these
            // entries are elided so the IPC surface is auditably narrower.
            #[cfg(feature = "teams")]
            commands::sync_team_library,
            #[cfg(feature = "teams")]
            commands::team_library_status,
            #[cfg(feature = "teams")]
            commands::list_team_snippets,
            // Server-backed personal-snippet sync (phase 4+).
            #[cfg(feature = "teams")]
            server_commands::server_auth_methods,
            #[cfg(feature = "teams")]
            server_commands::server_signup,
            #[cfg(feature = "teams")]
            server_commands::server_login,
            #[cfg(feature = "teams")]
            server_commands::server_logout,
            #[cfg(feature = "teams")]
            server_commands::server_status,
            #[cfg(feature = "teams")]
            server_commands::brand_defaults,
            #[cfg(feature = "teams")]
            server_commands::server_sync_now,
            #[cfg(feature = "teams")]
            server_commands::server_migrate_local_snippets,
            #[cfg(feature = "teams")]
            server_commands::server_trash_list,
            #[cfg(feature = "teams")]
            server_commands::server_trash_restore,
            #[cfg(feature = "teams")]
            server_commands::server_oidc_start_url,
            #[cfg(feature = "teams")]
            server_commands::server_oidc_paste_token,
            #[cfg(feature = "teams")]
            server_commands::server_update_profile,
            commands::capture_selection_for_snippet,
            commands::open_logs_folder,
            commands::open_backups_folder,
            commands::get_log_path,
        ])
        .run(tauri::generate_context!())
        .expect("error while running SnipDesk");
}

/// Toggle the main window and capture the prior foreground HWND for paste.
///
/// Tauri's `is_visible()` stays true while minimized, and "visible" doesn't
/// mean "frontmost" - the three flags (visible/minimized/focused) drive:
///   visible + !minimized + focused        -> hide
///   visible + !minimized + !focused       -> raise (buried behind another window)
///   minimized                             -> restore + focus
///   hidden                                -> show + focus
pub fn toggle_window_with_state(handle: &tauri::AppHandle, win: &tauri::WebviewWindow) {
    let is_visible = win.is_visible().unwrap_or(false);
    let is_minimized = win.is_minimized().unwrap_or(false);
    let is_focused = win.is_focused().unwrap_or(false);

    if is_visible && !is_minimized && is_focused {
        let _ = win.hide();
    } else if is_visible && !is_minimized {
        // Buried but on screen - set_focus is sufficient; no unminimize/show.
        let target = paste::capture_foreground_hwnd();
        if let Some(state) = handle.try_state::<AppState>() {
            state.target_hwnd.store(target, Ordering::SeqCst);
        }
        let _ = win.set_focus();
        let _ = win.emit("snipdesk://opened", ());
    } else {
        // Capture HWND before show; otherwise GetForegroundWindow returns ours.
        let target = paste::capture_foreground_hwnd();
        if let Some(state) = handle.try_state::<AppState>() {
            state.target_hwnd.store(target, Ordering::SeqCst);
            // Seed was_minimized=true: the restore animation fires Resized
            // bursts where is_minimized briefly reads true. Without this,
            // swap(prev=false, cur=true) misreads the burst as a fresh
            // minimize and triggers minimize-to-tray. The settled Resized
            // (cur=false) afterward swaps us back to the right state.
            state.was_minimized.store(true, Ordering::SeqCst);
        }
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
        // Re-sync the tracker once the animation has settled, otherwise the
        // next user-initiated minimize wouldn't register as a transition.
        let win_inner = win.clone();
        let state_handle = handle.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(250));
            let currently = win_inner.is_minimized().unwrap_or(false);
            if let Some(s) = state_handle.try_state::<AppState>() {
                s.was_minimized.store(currently, Ordering::SeqCst);
            }
        });
        // No explicit re-center: tauri-plugin-window-state restores the
        // user's last position from disk on launch, and we want subsequent
        // hotkey-toggles to respect whatever position they've dragged the
        // window to during this session. Re-centering every reopen wiped
        // their preference; users found this jarring.
        let _ = win.emit("snipdesk://opened", ());
    }
}

/// Show + unminimize + focus, with the same `was_minimized=true` pre-seed
/// as `toggle_window_with_state` to absorb the restore-animation Resized burst.
pub fn show_and_focus(handle: &tauri::AppHandle, win: &tauri::WebviewWindow) {
    if let Some(state) = handle.try_state::<AppState>() {
        state.was_minimized.store(true, Ordering::SeqCst);
    }
    let _ = win.unminimize();
    let _ = win.show();
    let _ = win.set_focus();

    let win_inner = win.clone();
    let state_handle = handle.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        let currently = win_inner.is_minimized().unwrap_or(false);
        if let Some(s) = state_handle.try_state::<AppState>() {
            s.was_minimized.store(currently, Ordering::SeqCst);
        }
    });
}

/// Returns true when the user is signed in to a server backend - the
/// `server_url` setting is non-empty AND a credential is stored for it.
/// While signed in, the server-side library sync owns the team_snippets
/// table; the legacy URL fetcher pauses so the two don't fight over it.
#[cfg(feature = "teams")]
fn signed_into_server(state: &AppState) -> bool {
    let url = match state.settings.lock() {
        Ok(g) => g.server_url.clone(),
        Err(_) => return false,
    };
    if url.trim().is_empty() {
        return false;
    }
    snipdesk_teams::credentials::load(&url)
        .map(|t| t.is_some())
        .unwrap_or(false)
}

/// Team-library refetch loop. Re-reads settings each iteration so URL/interval
/// edits take effect on the next tick; empty URL pauses (no error spam).
/// 30s sleep granularity so interval shortening is responsive.
///
/// **Coexistence with server library** (phase 5+): when the user is
/// signed in to a server, `snipdesk-teams::sync::tick` populates
/// team_snippets from `/api/library`; this legacy URL loop yields the
/// table to it by skipping every tick. The user can still leave a
/// `team_library_url` configured - it just won't run while signed in.
/// Signing out re-enables it on the next 30s pass.
#[cfg(feature = "teams")]
pub fn start_team_sync_thread(handle: tauri::AppHandle) {
    thread::spawn(move || {
        // Optional one-shot at startup so fresh snippets land before the
        // first interval tick. Skipped if signed in - the server library
        // sync thread handles that path.
        let did_startup_sync = {
            let state = match handle.try_state::<AppState>() {
                Some(s) => s,
                None => return,
            };
            if signed_into_server(&state) {
                false
            } else {
                let on_startup = state
                    .settings
                    .lock()
                    .map(|g| g.team_library_sync_on_startup && !g.team_library_url.is_empty())
                    .unwrap_or(false);
                if on_startup {
                    run_one_team_sync(&handle);
                    true
                } else {
                    false
                }
            }
        };
        if did_startup_sync {
            // Cosmetic: let initial results paint before the next pass.
            thread::sleep(Duration::from_secs(5));
        }

        // Wall-clock so laptop suspend doesn't extend the sleep past the interval.
        let mut last_sync = SystemTime::now();
        loop {
            thread::sleep(Duration::from_secs(30));
            let (url_empty, interval_mins, signed_in) = {
                let state = match handle.try_state::<AppState>() {
                    Some(s) => s,
                    None => return,
                };
                let signed_in = signed_into_server(&state);
                let g = match state.settings.lock() {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                (
                    g.team_library_url.trim().is_empty(),
                    g.team_library_sync_interval_mins.max(1) as u64,
                    signed_in,
                )
            };
            if signed_in || url_empty {
                continue;
            }
            let elapsed = SystemTime::now()
                .duration_since(last_sync)
                .unwrap_or(Duration::ZERO);
            if elapsed >= Duration::from_secs(interval_mins * 60) {
                run_one_team_sync(&handle);
                last_sync = SystemTime::now();
            }
        }
    });
}

/// One sync pass: fetch URL, replace team_snippets, update status atomics,
/// emit `snipdesk://team-library-updated`.
#[cfg(feature = "teams")]
pub fn run_one_team_sync(handle: &tauri::AppHandle) {
    let state = match handle.try_state::<AppState>() {
        Some(s) => s,
        None => return,
    };
    let url = match state.settings.lock() {
        Ok(g) => g.team_library_url.clone(),
        Err(_) => return,
    };
    if url.trim().is_empty() {
        return;
    }

    match shared_url::fetch(&url) {
        Ok(lib) => {
            let count = match state.db.lock() {
                Ok(db) => match db.replace_team_snippets(&lib.snippets) {
                    Ok(n) => n,
                    Err(err) => {
                        logging::log_error(&format!(
                            "team sync: replace_team_snippets failed: {err}"
                        ));
                        0
                    }
                },
                Err(_) => 0,
            };
            state.team_snippet_count.store(count, Ordering::SeqCst);
            state.team_last_fetched_unix.store(
                shared_library::system_time_to_unix(SystemTime::now()),
                Ordering::SeqCst,
            );
            if let Ok(mut e) = state.team_last_error.lock() {
                *e = None;
            }
            logging::log_info(&format!("team sync: merged {count} snippets from {url}"));
            let _ = handle.emit("snipdesk://team-library-updated", ());
        }
        Err(err) => {
            logging::log_error(&format!("team sync: {err}"));
            if let Ok(mut e) = state.team_last_error.lock() {
                *e = Some(err);
            }
        }
    }
}

/// Capture OS selection, open editor with prefill. Non-Windows: stub -
/// the save-clipboard / Ctrl+C / poll / restore dance is Win32-only.
pub fn trigger_quick_add_from_selection(handle: &tauri::AppHandle) {
    #[cfg(windows)]
    {
        let handle_clone = handle.clone();
        thread::spawn(move || {
            // Off the shortcut thread pool - blocking it drops subsequent presses.
            let captured = paste::capture_selection_windows();
            match captured {
                Ok(Some(text)) if !text.trim().is_empty() => {
                    if let Some(win) = handle_clone.get_webview_window("main") {
                        show_and_focus(&handle_clone, &win);
                        let _ = win.emit("snipdesk://quick-add", text);
                    }
                }
                Ok(_) => {
                    // Empty selection - open editor with no prefill rather
                    // than swallow the hotkey silently.
                    if let Some(win) = handle_clone.get_webview_window("main") {
                        show_and_focus(&handle_clone, &win);
                        let _ = win.emit("snipdesk://open-editor", ());
                    }
                }
                Err(err) => {
                    logging::log_error(&format!("quick-add capture failed: {err}"));
                    if let Some(win) = handle_clone.get_webview_window("main") {
                        show_and_focus(&handle_clone, &win);
                        let _ = win.emit("snipdesk://open-editor", ());
                    }
                }
            }
        });
    }
    #[cfg(not(windows))]
    {
        if let Some(win) = handle.get_webview_window("main") {
            show_and_focus(handle, &win);
            let _ = win.emit("snipdesk://open-editor", ());
        }
    }
}

/// Sync the OS login item to `enabled`.
pub fn apply_autostart(handle: &tauri::AppHandle, enabled: bool) -> tauri::Result<()> {
    let autolaunch = handle.autolaunch();
    let currently = autolaunch.is_enabled().unwrap_or(false);
    if enabled && !currently {
        let _ = autolaunch.enable();
    } else if !enabled && currently {
        let _ = autolaunch.disable();
    }
    Ok(())
}

/// True while a Settings hotkey-capture field has focus (see
/// `AppState::hotkeys_suspended`). Shared by every global-shortcut
/// handler so a chord typed into the capture field never doubles as
/// the action it's bound to.
pub fn hotkeys_are_suspended(handle: &tauri::AppHandle) -> bool {
    handle
        .try_state::<AppState>()
        .map(|s| s.hotkeys_suspended.load(Ordering::SeqCst))
        .unwrap_or(false)
}

/// Pretty-print a hotkey for the tray. Falls back to the input on parse failure.
fn friendly_shortcut(hk: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for raw in hk.split('+') {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        let lc = t.to_ascii_lowercase();
        let nice = match lc.as_str() {
            "commandorcontrol" | "cmdorctrl" => {
                #[cfg(target_os = "macos")]
                {
                    "Cmd".to_string()
                }
                #[cfg(not(target_os = "macos"))]
                {
                    "Ctrl".to_string()
                }
            }
            "control" | "ctrl" => "Ctrl".to_string(),
            "command" | "cmd" | "super" | "meta" => "Cmd".to_string(),
            "option" | "alt" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            other => {
                // Single char -> upper; word -> title case.
                if other.len() == 1 {
                    other.to_ascii_uppercase()
                } else {
                    let mut chars = other.chars();
                    match chars.next() {
                        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
                        None => String::new(),
                    }
                }
            }
        };
        parts.push(nice);
    }
    if parts.is_empty() {
        hk.to_string()
    } else {
        parts.join("+")
    }
}

/// Parse shortcut strings like "CommandOrControl+Shift+Space" or "Alt+F1".
fn parse_shortcut(s: &str) -> Option<Shortcut> {
    let mut mods = Modifiers::empty();
    let mut key: Option<Code> = None;

    for raw in s.split('+') {
        let token = raw.trim();
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "commandorcontrol" | "cmdorctrl" => {
                mods |= Modifiers::CONTROL;
            }
            "cmd" | "command" | "super" | "meta" => {
                mods |= Modifiers::SUPER;
            }
            "shift" => mods |= Modifiers::SHIFT,
            "alt" | "option" => mods |= Modifiers::ALT,
            other => {
                key = code_from_str(other);
            }
        }
    }

    key.map(|k| Shortcut::new(Some(mods), k))
}

fn code_from_str(s: &str) -> Option<Code> {
    use Code::*;
    let up = s.to_ascii_uppercase();
    Some(match up.as_str() {
        "SPACE" => Space,
        "ENTER" | "RETURN" => Enter,
        "TAB" => Tab,
        "ESCAPE" | "ESC" => Escape,
        "BACKSPACE" => Backspace,
        "UP" => ArrowUp,
        "DOWN" => ArrowDown,
        "LEFT" => ArrowLeft,
        "RIGHT" => ArrowRight,
        "F1" => F1,
        "F2" => F2,
        "F3" => F3,
        "F4" => F4,
        "F5" => F5,
        "F6" => F6,
        "F7" => F7,
        "F8" => F8,
        "F9" => F9,
        "F10" => F10,
        "F11" => F11,
        "F12" => F12,
        s if s.len() == 1 => match s.chars().next().unwrap() {
            'A' => KeyA,
            'B' => KeyB,
            'C' => KeyC,
            'D' => KeyD,
            'E' => KeyE,
            'F' => KeyF,
            'G' => KeyG,
            'H' => KeyH,
            'I' => KeyI,
            'J' => KeyJ,
            'K' => KeyK,
            'L' => KeyL,
            'M' => KeyM,
            'N' => KeyN,
            'O' => KeyO,
            'P' => KeyP,
            'Q' => KeyQ,
            'R' => KeyR,
            'S' => KeyS,
            'T' => KeyT,
            'U' => KeyU,
            'V' => KeyV,
            'W' => KeyW,
            'X' => KeyX,
            'Y' => KeyY,
            'Z' => KeyZ,
            '0' => Digit0,
            '1' => Digit1,
            '2' => Digit2,
            '3' => Digit3,
            '4' => Digit4,
            '5' => Digit5,
            '6' => Digit6,
            '7' => Digit7,
            '8' => Digit8,
            '9' => Digit9,
            _ => return None,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The hotkey parser is the one place where typos in user-facing strings
    // ("Alt+Space") become silent runtime failures (no hotkey registers, the
    // app feels broken). Everything else in this file is plumbing where a
    // compile-time error would catch the bug.
    #[test]
    fn parse_shortcut_accepts_canonical_alt_space() {
        assert!(parse_shortcut("Alt+Space").is_some());
        assert!(parse_shortcut("Ctrl+Shift+P").is_some());
        assert!(parse_shortcut("CommandOrControl+F12").is_some());
    }

    #[test]
    fn parse_shortcut_rejects_invalid_input() {
        assert!(parse_shortcut("definitely+not+a+key").is_none());
        assert!(parse_shortcut("").is_none());
        // Modifier-only - there's no key code, so the result should be None.
        assert!(parse_shortcut("Ctrl+Shift").is_none());
    }
}
