use std::collections::HashMap;
use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_global_shortcut::GlobalShortcutExt;

use crate::db::{FolderInfo, ImportResult, NewSnippet, Snippet, SortOrder, UpdateSnippet};
use crate::paste;
use crate::settings::{Settings, SettingsPath};
#[cfg(feature = "teams")]
use crate::shared_library::SyncStatus;
use crate::AppState;

pub type CmdResult<T> = std::result::Result<T, String>;

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

#[tauri::command]
pub fn list_snippets(
    state: State<'_, AppState>,
    query: Option<String>,
    tag: Option<String>,
    folder: Option<String>,
    sort: Option<SortOrder>,
) -> CmdResult<Vec<Snippet>> {
    let db = state.db.lock().map_err(e)?;
    // Default to the saved preference so the frontend can omit `sort`.
    let sort = sort.unwrap_or_else(|| {
        let s = state.settings.lock();
        match s {
            Ok(g) => {
                if g.sort_by_usage {
                    SortOrder::Usage
                } else {
                    SortOrder::Alphabetical
                }
            }
            Err(_) => SortOrder::Usage,
        }
    });
    db.list(query.as_deref(), tag.as_deref(), folder.as_deref(), sort)
        .map_err(e)
}

#[tauri::command]
pub fn get_snippet(state: State<'_, AppState>, id: String) -> CmdResult<Option<Snippet>> {
    let db = state.db.lock().map_err(e)?;
    // `team:` prefix routes to team_snippets; frontend just asks by id.
    if let Some(team_id) = id.strip_prefix("team:") {
        return db.get_team_snippet(team_id).map_err(e);
    }
    db.get(&id).map_err(e)
}

#[tauri::command]
pub fn create_snippet(state: State<'_, AppState>, input: NewSnippet) -> CmdResult<Snippet> {
    let db = state.db.lock().map_err(e)?;
    db.create(input).map_err(e)
}

#[tauri::command]
pub fn update_snippet(
    state: State<'_, AppState>,
    id: String,
    input: UpdateSnippet,
) -> CmdResult<Snippet> {
    let db = state.db.lock().map_err(e)?;
    db.update(&id, input).map_err(e)
}

#[tauri::command]
pub fn delete_snippet(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let db = state.db.lock().map_err(e)?;
    db.delete(&id).map_err(e)
}

#[tauri::command]
pub fn list_tags(state: State<'_, AppState>) -> CmdResult<Vec<String>> {
    let db = state.db.lock().map_err(e)?;
    db.list_tags().map_err(e)
}

#[tauri::command]
pub fn duplicate_snippet(state: State<'_, AppState>, id: String) -> CmdResult<Snippet> {
    let db = state.db.lock().map_err(e)?;
    db.duplicate(&id).map_err(e)
}

// ---- Folders ----

#[tauri::command]
pub fn list_folders(state: State<'_, AppState>) -> CmdResult<Vec<FolderInfo>> {
    let db = state.db.lock().map_err(e)?;
    db.list_folders().map_err(e)
}

#[derive(Debug, Deserialize)]
pub struct CreateFolderArgs {
    pub path: String,
}

#[tauri::command]
pub fn create_folder(state: State<'_, AppState>, args: CreateFolderArgs) -> CmdResult<()> {
    let db = state.db.lock().map_err(e)?;
    db.create_folder(&args.path).map_err(e)
}

#[derive(Debug, Deserialize)]
pub struct RenameFolderArgs {
    pub old_path: String,
    pub new_path: String,
}

#[tauri::command]
pub fn rename_folder(state: State<'_, AppState>, args: RenameFolderArgs) -> CmdResult<()> {
    let db = state.db.lock().map_err(e)?;
    db.rename_folder(&args.old_path, &args.new_path).map_err(e)
}

#[derive(Debug, Deserialize)]
pub struct DeleteFolderArgs {
    pub path: String,
    /// true: cascade delete; false: promote contained snippets to root.
    #[serde(default)]
    pub delete_snippets: bool,
}

#[tauri::command]
pub fn delete_folder(state: State<'_, AppState>, args: DeleteFolderArgs) -> CmdResult<()> {
    let db = state.db.lock().map_err(e)?;
    db.delete_folder(&args.path, args.delete_snippets)
        .map_err(e)
}

// ---- Variable autosuggest history ----

#[derive(Debug, Deserialize)]
pub struct VarHistoryArgs {
    pub snippet_id: String,
    pub var_names: Vec<String>,
}

#[tauri::command]
pub fn get_var_history(
    state: State<'_, AppState>,
    args: VarHistoryArgs,
) -> CmdResult<HashMap<String, Vec<String>>> {
    let db = state.db.lock().map_err(e)?;
    db.get_var_history(&args.snippet_id, &args.var_names)
        .map_err(e)
}

#[derive(Debug, Deserialize)]
pub struct UseSnippetArgs {
    pub id: String,
    #[serde(default)]
    pub variables: HashMap<String, String>,
    /// Override settings.paste_mode for this call. "clipboard" | "auto_paste"
    pub paste_mode: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UseSnippetResult {
    pub rendered: String,
    pub pasted: bool,
}

#[tauri::command]
pub fn use_snippet(
    app: AppHandle,
    state: State<'_, AppState>,
    args: UseSnippetArgs,
) -> CmdResult<UseSnippetResult> {
    // Team snippets are read-only and live in a separate table that's
    // wholly replaced each sync - recording usage there would be lost.
    let (body, settings) = {
        let db = state.db.lock().map_err(e)?;
        let snippet = if let Some(team_id) = args.id.strip_prefix("team:") {
            db.get_team_snippet(team_id)
                .map_err(e)?
                .ok_or_else(|| "team snippet not found".to_string())?
        } else {
            let s = db
                .get(&args.id)
                .map_err(e)?
                .ok_or_else(|| "snippet not found".to_string())?;
            db.record_use(&args.id).map_err(e)?;
            if !args.variables.is_empty() {
                if let Err(err) = db.record_variable_values(&args.id, &args.variables) {
                    eprintln!("var history record failed: {err}");
                }
            }
            s
        };
        let settings = state.settings.lock().map_err(e)?.clone();
        (snippet.body, settings)
    };

    let rendered = substitute_variables(&body, &args.variables);

    // Windows: write CF_UNICODETEXT directly. The plugin's arboard path
    // mangled non-ASCII (em dash, curly quotes) into UTF-8-as-Windows-1252
    // mojibake. macOS/Linux still go through arboard.
    #[cfg(windows)]
    {
        if let Err(err) = paste::write_clipboard_unicode(&rendered) {
            // Fall back to plugin if e.g. clipboard is locked by another process.
            eprintln!("direct clipboard write failed, falling back: {err}");
            app.clipboard().write_text(rendered.clone()).map_err(e)?;
        }
    }
    #[cfg(not(windows))]
    {
        app.clipboard().write_text(rendered.clone()).map_err(e)?;
    }

    let mode = args
        .paste_mode
        .unwrap_or_else(|| settings.paste_mode.clone());
    let pasted = if mode == "auto_paste" {
        // Hide first so focus starts returning; the paste worker re-asserts
        // focus before typing rather than racing Windows' restore - required
        // for the variable-prompt path where we've held focus long enough
        // that auto-restore can miss the target.
        let target = state.target_hwnd.load(Ordering::SeqCst);
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.hide();
        }
        paste::trigger_paste(settings.auto_paste_delay_ms, target);
        true
    } else {
        if settings.close_on_paste {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.hide();
            }
        }
        false
    };

    Ok(UseSnippetResult { rendered, pasted })
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> CmdResult<Settings> {
    let s = state.settings.lock().map_err(e)?;
    Ok(s.clone())
}

#[tauri::command]
pub fn update_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    new_settings: Settings,
) -> CmdResult<Settings> {
    // Snapshot prior values so we only re-apply on actual change.
    let (old_hotkey, old_quick_add, old_start_with_windows, old_always_on_top) = {
        let s = state.settings.lock().map_err(e)?;
        (
            s.hotkey.clone(),
            s.quick_add_hotkey.clone(),
            s.start_with_windows,
            s.always_on_top,
        )
    };

    if old_hotkey != new_settings.hotkey {
        let shortcut_old = crate::parse_shortcut(&old_hotkey);
        let shortcut_new = crate::parse_shortcut(&new_settings.hotkey)
            .ok_or_else(|| format!("invalid hotkey: {}", new_settings.hotkey))?;

        if let Some(sc) = shortcut_old {
            let _ = app.global_shortcut().unregister(sc);
        }

        let handle = app.clone();
        app.global_shortcut()
            .on_shortcut(shortcut_new, move |_app, _sc, event| {
                if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                    if let Some(win) = handle.get_webview_window("main") {
                        // Routed through the toggle helper so the new hotkey
                        // still captures the target HWND for paste.
                        crate::toggle_window_with_state(&handle, &win);
                    }
                }
            })
            .map_err(e)?;
    }

    // Quick-add: re-register on change. Empty disables; malformed logs but
    // doesn't fail the save (don't strand the user on a typo).
    if old_quick_add != new_settings.quick_add_hotkey {
        if let Some(sc) = crate::parse_shortcut(&old_quick_add) {
            let _ = app.global_shortcut().unregister(sc);
        }
        if !new_settings.quick_add_hotkey.trim().is_empty() {
            if let Some(sc_new) = crate::parse_shortcut(&new_settings.quick_add_hotkey) {
                let handle = app.clone();
                if let Err(err) =
                    app.global_shortcut()
                        .on_shortcut(sc_new, move |_app, _sc, event| {
                            if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed
                            {
                                crate::trigger_quick_add_from_selection(&handle);
                            }
                        })
                {
                    eprintln!("quick-add re-register failed: {err}");
                }
            } else {
                eprintln!(
                    "quick-add hotkey not recognized: {}",
                    new_settings.quick_add_hotkey
                );
            }
        }
    }

    if old_start_with_windows != new_settings.start_with_windows {
        if let Err(err) = crate::apply_autostart(&app, new_settings.start_with_windows) {
            eprintln!("failed to update autostart: {err}");
        }
    }

    // Live-apply always-on-top.
    if old_always_on_top != new_settings.always_on_top {
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.set_always_on_top(new_settings.always_on_top);
        }
    }

    let path = app.state::<SettingsPath>().0.clone();
    new_settings.save(&path).map_err(e)?;
    {
        let mut s = state.settings.lock().map_err(e)?;
        *s = new_settings.clone();
    }
    Ok(new_settings)
}

#[derive(Debug, Deserialize)]
pub struct ExportArgs {
    pub path: String,
    pub format: String, // "json" | "csv"
}

#[tauri::command]
pub fn export_snippets(state: State<'_, AppState>, args: ExportArgs) -> CmdResult<usize> {
    let snippets = {
        let db = state.db.lock().map_err(e)?;
        db.export_all().map_err(e)?
    };

    match args.format.as_str() {
        "json" => {
            let json = serde_json::to_string_pretty(&snippets).map_err(e)?;
            std::fs::write(&args.path, json).map_err(e)?;
        }
        "csv" => {
            let mut out = String::from("title,body,tags\n");
            for s in &snippets {
                out.push_str(&format!(
                    "{},{},{}\n",
                    csv_field(&s.title),
                    csv_field(&s.body),
                    csv_field(&s.tags.join(";"))
                ));
            }
            std::fs::write(&args.path, out).map_err(e)?;
        }
        other => return Err(format!("unsupported format: {other}")),
    }

    Ok(snippets.len())
}

#[derive(Debug, Deserialize)]
pub struct ImportArgs {
    pub path: String,
    pub format: String, // "json" | "csv"
}

#[tauri::command]
pub fn import_snippets(state: State<'_, AppState>, args: ImportArgs) -> CmdResult<ImportResult> {
    let items: Vec<NewSnippet> = match args.format.as_str() {
        "json" => {
            let contents = std::fs::read_to_string(&args.path).map_err(e)?;
            // Accept NewSnippet[] or full Snippet[] (the export_snippets shape).
            match serde_json::from_str::<Vec<NewSnippet>>(&contents) {
                Ok(v) => v,
                Err(_) => {
                    let full: Vec<Snippet> = serde_json::from_str(&contents).map_err(e)?;
                    full.into_iter()
                        .map(|s| NewSnippet {
                            title: s.title,
                            body: s.body,
                            tags: s.tags,
                            folder_path: s.folder_path,
                        })
                        .collect()
                }
            }
        }
        "csv" => {
            let contents = std::fs::read_to_string(&args.path).map_err(e)?;
            parse_csv(&contents).map_err(e)?
        }
        other => return Err(format!("unsupported format: {other}")),
    };

    let db = state.db.lock().map_err(e)?;
    db.import(items).map_err(e)
}

#[tauri::command]
pub fn hide_window(app: AppHandle) -> CmdResult<()> {
    if let Some(win) = app.get_webview_window("main") {
        win.hide().map_err(e)?;
    }
    Ok(())
}

/// Frontend calls this before opening a native file dialog so blur-hide
/// doesn't dismiss the launcher when the dialog steals focus.
#[tauri::command]
pub fn suspend_hide_on_blur(state: State<'_, AppState>) -> CmdResult<()> {
    state.hide_on_blur_suppressed.store(true, Ordering::SeqCst);
    Ok(())
}

/// Pair to `suspend_hide_on_blur`; called once the dialog resolves.
#[tauri::command]
pub fn resume_hide_on_blur(state: State<'_, AppState>) -> CmdResult<()> {
    state.hide_on_blur_suppressed.store(false, Ordering::SeqCst);
    Ok(())
}

// ---- Duplicate-title detection ----

#[derive(Debug, Deserialize)]
pub struct CheckTitleArgs {
    pub title: String,
    /// Exclude this id so editing-without-renaming doesn't self-conflict.
    #[serde(default)]
    pub exclude_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TitleConflict {
    /// Trim + case-insensitive match against another local snippet.
    pub conflict: bool,
    pub existing_id: Option<String>,
    pub existing_title: Option<String>,
    pub existing_folder: Option<String>,
}

#[tauri::command]
pub fn check_title_conflict(
    state: State<'_, AppState>,
    args: CheckTitleArgs,
) -> CmdResult<TitleConflict> {
    let db = state.db.lock().map_err(e)?;
    let hit = db
        .find_by_title(&args.title, args.exclude_id.as_deref())
        .map_err(e)?;
    Ok(match hit {
        Some(s) => TitleConflict {
            conflict: true,
            existing_id: Some(s.id),
            existing_title: Some(s.title),
            existing_folder: s.folder_path,
        },
        None => TitleConflict {
            conflict: false,
            existing_id: None,
            existing_title: None,
            existing_folder: None,
        },
    })
}

// ---- Team library (Teams build only) ----
//
// IPC for the shared-URL fetcher: sync now, status, list. Whole block is
// gated so the free build's IPC handler doesn't reference these names.
// Calls in a free build return "command not found" - JS side treats that
// as a no-op.

/// Manual "Sync now". Runs on the command thread so the frontend can await
/// it and read the resulting status; the scheduled loop only logs.
#[cfg(feature = "teams")]
#[tauri::command]
pub fn sync_team_library(app: AppHandle) -> CmdResult<SyncStatus> {
    crate::run_one_team_sync(&app);
    // Returns last-known status either way; `last_error` distinguishes ok vs fail.
    team_library_status(app.state::<AppState>())
}

#[cfg(feature = "teams")]
#[tauri::command]
pub fn team_library_status(state: State<'_, AppState>) -> CmdResult<SyncStatus> {
    let fetched = state.team_last_fetched_unix.load(Ordering::SeqCst);
    let snippet_count = state.team_snippet_count.load(Ordering::SeqCst);
    let last_error = state.team_last_error.lock().map_err(e)?.clone();
    Ok(SyncStatus {
        fetched_at_unix: if fetched == 0 { None } else { Some(fetched) },
        snippet_count,
        last_error,
    })
}

#[cfg(feature = "teams")]
#[tauri::command]
pub fn list_team_snippets(state: State<'_, AppState>) -> CmdResult<Vec<Snippet>> {
    let db = state.db.lock().map_err(e)?;
    db.list_team_snippets().map_err(e)
}

// ---- Quick add from selection ----

/// UI-button entry point for the same flow as the quick-add global hotkey.
#[tauri::command]
pub fn capture_selection_for_snippet(app: AppHandle) -> CmdResult<()> {
    crate::trigger_quick_add_from_selection(&app);
    Ok(())
}

// ---- Filesystem reveal buttons ----

/// Absolute path to `snipdesk.log` for the settings panel.
#[tauri::command]
pub fn get_log_path() -> CmdResult<Option<String>> {
    Ok(crate::logging::log_path().map(|p| p.to_string_lossy().into_owned()))
}

#[tauri::command]
pub fn open_logs_folder(app: AppHandle) -> CmdResult<()> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("no app data dir: {err}"))?;
    let logs_dir = data_dir.join("logs");
    std::fs::create_dir_all(&logs_dir).ok();
    reveal_in_explorer(&logs_dir).map_err(e)
}

#[tauri::command]
pub fn open_backups_folder(app: AppHandle) -> CmdResult<()> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("no app data dir: {err}"))?;
    let backups_dir = crate::backup::backups_dir(&data_dir);
    std::fs::create_dir_all(&backups_dir).ok();
    reveal_in_explorer(&backups_dir).map_err(e)
}

/// Open `path` in the OS file manager. explorer / open / xdg-open.
fn reveal_in_explorer(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer").arg(path).spawn()?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(path).spawn()?;
        Ok(())
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn()?;
        Ok(())
    }
}

// ---- helpers ----

fn substitute_variables(body: &str, vars: &HashMap<String, String>) -> String {
    // Replace `{name}` with vars["name"] when present, leave intact otherwise.
    //
    // Must operate on &str slices, not bytes - the previous byte-loop pushed
    // each UTF-8 byte as a Latin-1 char, turning `-` (E2 80 94) into `â` plus
    // garbage and producing the em-dash-on-paste mojibake bug.
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(open_rel) = rest.find('{') {
        out.push_str(&rest[..open_rel]);
        let after_open = &rest[open_rel + '{'.len_utf8()..];

        // Unknown / malformed placeholders are emitted verbatim rather than dropped.
        if let Some(close_rel) = after_open.find('}') {
            let name = &after_open[..close_rel];
            if is_valid_var_name(name) {
                if let Some(val) = vars.get(name) {
                    out.push_str(val);
                    rest = &after_open[close_rel + '}'.len_utf8()..];
                    continue;
                }
            }
        }
        out.push('{');
        rest = after_open;
    }
    out.push_str(rest);
    out
}

fn is_valid_var_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn parse_csv(contents: &str) -> anyhow::Result<Vec<NewSnippet>> {
    // RFC-4180-ish: quoted fields, embedded commas/newlines, "" escape.
    // Header: title,body,tags
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut cur_row: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = contents.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => {
                    cur_row.push(std::mem::take(&mut cur));
                }
                '\n' => {
                    cur_row.push(std::mem::take(&mut cur));
                    rows.push(std::mem::take(&mut cur_row));
                }
                '\r' => { /* skip */ }
                _ => cur.push(c),
            }
        }
    }
    if !cur.is_empty() || !cur_row.is_empty() {
        cur_row.push(cur);
        rows.push(cur_row);
    }

    if rows.is_empty() {
        return Ok(vec![]);
    }

    let header = rows.remove(0);
    let find = |name: &str| {
        header
            .iter()
            .position(|h| h.trim().eq_ignore_ascii_case(name))
    };
    let title_idx = find("title").ok_or_else(|| anyhow::anyhow!("missing 'title' column"))?;
    let body_idx = find("body").ok_or_else(|| anyhow::anyhow!("missing 'body' column"))?;
    let tags_idx = find("tags");

    let mut out = Vec::new();
    for row in rows {
        if row.iter().all(|c| c.trim().is_empty()) {
            continue;
        }
        let title = row.get(title_idx).cloned().unwrap_or_default();
        let body = row.get(body_idx).cloned().unwrap_or_default();
        let tags = tags_idx
            .and_then(|i| row.get(i).cloned())
            .map(|s| {
                s.split(&[';', ','][..])
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !title.trim().is_empty() {
            out.push(NewSnippet {
                title,
                body,
                tags,
                folder_path: None,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // Happy path. If this breaks, half the app is broken.
    #[test]
    fn substitutes_known_placeholders() {
        let v = vars(&[("name", "Alex"), ("ticket", "5678")]);
        assert_eq!(
            substitute_variables("Hi {name}, ref #{ticket}", &v),
            "Hi Alex, ref #5678"
        );
    }

    // Regression guard. The byte-loop implementation pushed individual UTF-8
    // bytes as Latin-1 chars, turning `-` (E2 80 94) into `â` plus garbage.
    // This test pins the fix; without it, future refactors of substitute_variables
    // could silently re-introduce the mojibake.
    #[test]
    fn unicode_passes_through_unchanged() {
        let v = vars(&[("name", "François")]);
        let body = "Hi {name} \u{2014} \u{2013} \u{2018}quote\u{2019} ñ é 中文";
        let expected = "Hi François \u{2014} \u{2013} \u{2018}quote\u{2019} ñ é 中文";
        assert_eq!(substitute_variables(body, &v), expected);
    }
}
