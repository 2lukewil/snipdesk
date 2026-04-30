use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Default "Alt+Space". Ctrl chords collide with command palettes in
    /// many ticketing tools.
    pub hotkey: String,
    /// "clipboard" or "auto_paste"
    pub paste_mode: String,
    /// Delay before synthesizing Ctrl+V — lets the window finish closing.
    pub auto_paste_delay_ms: u64,
    pub close_on_paste: bool,
    pub sort_by_usage: bool,
    #[serde(default = "default_true")]
    pub start_with_windows: bool,
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    #[serde(default = "default_true")]
    pub minimize_to_tray: bool,
    /// `--autostart` always starts hidden regardless of this.
    #[serde(default)]
    pub start_in_tray: bool,
    #[serde(default)]
    pub hide_on_blur: bool,
    #[serde(default)]
    pub always_on_top: bool,
    /// "dark" | "light" | "system".
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Lowercase CSS hex like "#4c9aff". Empty = use the theme's built-in
    /// accent. Frontend normalizes hex / rgb() / picker input before sending.
    #[serde(default)]
    pub accent_color: String,
    #[serde(default)]
    pub compact: bool,
    #[serde(default = "default_true")]
    pub show_usage_count: bool,
    /// Off by default; needs typing_speed_wpm / hourly_wage to be meaningful.
    #[serde(default)]
    pub show_savings_estimate: bool,
    /// 40 is population average; support agents self-report 55–75.
    #[serde(default = "default_wpm")]
    pub typing_speed_wpm: u32,
    /// 0 = show time saved only, no money.
    #[serde(default)]
    pub hourly_wage: f64,
    #[serde(default = "default_currency")]
    pub wage_currency: String,
    #[serde(default)]
    pub onboarding_completed: bool,

    // ---- Quick-add-from-selection ----
    /// Empty = disabled.
    #[serde(default = "default_quick_add_hotkey")]
    pub quick_add_hotkey: String,

    // ---- Team library (pull-only URL sync) ----
    /// JSON document URL. Empty = disabled (default).
    #[serde(default)]
    pub team_library_url: String,
    #[serde(default = "default_team_sync_interval")]
    pub team_library_sync_interval_mins: u32,
    #[serde(default = "default_true")]
    pub team_library_sync_on_startup: bool,
    /// Localizable for non-English UIs.
    #[serde(default = "default_team_folder_name")]
    pub team_library_folder_name: String,

    // ---- Editor formatting toolbar ----
    /// User-customizable; teams ship different markup (Markdown, BBCode, etc).
    #[serde(default = "default_format_rules")]
    pub format_rules: Vec<FormatRule>,

    // ---- Retention knobs ----
    #[serde(default = "default_backup_retention_days")]
    pub backup_retention_days: u32,
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: u32,
}

/// `prefix`/`suffix` wrap the current selection (or cursor position).
/// e.g. Bold = `("**", "**")`, Link = `("[", "](https://)")`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatRule {
    pub label: String,
    pub prefix: String,
    pub suffix: String,
}

fn default_wpm() -> u32 {
    40
}

fn default_currency() -> String {
    "$".to_string()
}

fn default_theme() -> String {
    "dark".to_string()
}

fn default_true() -> bool {
    true
}

fn default_quick_add_hotkey() -> String {
    "Alt+Shift+Space".to_string()
}

fn default_team_sync_interval() -> u32 {
    60
}

fn default_team_folder_name() -> String {
    "Team Library".to_string()
}

fn default_backup_retention_days() -> u32 {
    14
}

fn default_log_retention_days() -> u32 {
    7
}

/// Tuned for WHMCS ticket replies (Markdown-ish).
fn default_format_rules() -> Vec<FormatRule> {
    vec![
        FormatRule {
            label: "Bold".into(),
            prefix: "**".into(),
            suffix: "**".into(),
        },
        FormatRule {
            label: "Italic".into(),
            prefix: "*".into(),
            suffix: "*".into(),
        },
        FormatRule {
            label: "Code".into(),
            prefix: "`".into(),
            suffix: "`".into(),
        },
        FormatRule {
            label: "Link".into(),
            prefix: "[".into(),
            suffix: "](https://)".into(),
        },
    ]
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey: "Alt+Space".into(),
            paste_mode: "auto_paste".into(),
            auto_paste_delay_ms: 120,
            close_on_paste: true,
            sort_by_usage: true,
            start_with_windows: true,
            close_to_tray: true,
            minimize_to_tray: true,
            start_in_tray: false,
            hide_on_blur: false,
            always_on_top: false,
            theme: "dark".to_string(),
            accent_color: String::new(),
            compact: false,
            show_usage_count: true,
            show_savings_estimate: false,
            typing_speed_wpm: 40,
            hourly_wage: 0.0,
            wage_currency: "$".to_string(),
            onboarding_completed: false,
            quick_add_hotkey: default_quick_add_hotkey(),
            team_library_url: String::new(),
            team_library_sync_interval_mins: default_team_sync_interval(),
            team_library_sync_on_startup: true,
            team_library_folder_name: default_team_folder_name(),
            format_rules: default_format_rules(),
            backup_retention_days: default_backup_retention_days(),
            log_retention_days: default_log_retention_days(),
        }
    }
}

impl Settings {
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(path, s)?;
        Ok(())
    }
}

pub struct SettingsPath(pub PathBuf);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_documented_values() {
        let s = Settings::default();
        assert_eq!(s.hotkey, "Alt+Space");
        assert_eq!(s.paste_mode, "auto_paste");
        assert!(s.minimize_to_tray);
        assert!(s.start_with_windows);
        assert_eq!(s.theme, "dark");
        assert_eq!(s.team_library_folder_name, "Team Library");
        assert!(!s.format_rules.is_empty());
    }

    #[test]
    fn round_trips_through_serde() {
        let original = Settings::default();
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: Settings = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.hotkey, original.hotkey);
        assert_eq!(parsed.theme, original.theme);
        assert_eq!(parsed.format_rules.len(), original.format_rules.len());
    }

    #[test]
    fn legacy_settings_json_loads_with_defaults_for_new_fields() {
        // Pre-Teams settings.json had no team_library_*, no format_rules,
        // no quick_add_hotkey. Loading must fall back to defaults rather
        // than erroring — this is the agent-upgrade contract.
        let legacy = r#"{
            "hotkey": "Ctrl+Shift+Space",
            "paste_mode": "clipboard",
            "auto_paste_delay_ms": 50,
            "close_on_paste": false,
            "sort_by_usage": false
        }"#;
        let parsed: Settings = serde_json::from_str(legacy).expect("legacy load");
        assert_eq!(parsed.hotkey, "Ctrl+Shift+Space");
        assert_eq!(parsed.paste_mode, "clipboard");
        assert!(parsed.minimize_to_tray);
        assert_eq!(parsed.team_library_url, "");
        assert_eq!(parsed.team_library_folder_name, "Team Library");
        assert_eq!(parsed.quick_add_hotkey, "Alt+Shift+Space");
        assert!(!parsed.format_rules.is_empty());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        // serde's default behaviour, but lock it in — adding a field on
        // the server side shouldn't make older clients refuse to load.
        let json = r#"{
            "hotkey":"Alt+Space","paste_mode":"auto_paste",
            "auto_paste_delay_ms":120,"close_on_paste":true,
            "sort_by_usage":true,"future_unknown_field":"x"
        }"#;
        let parsed: Settings = serde_json::from_str(json).expect("ignore unknown");
        assert_eq!(parsed.hotkey, "Alt+Space");
    }

    #[test]
    fn load_or_default_returns_default_for_missing_file() {
        let nonexistent = std::path::Path::new("/__definitely_not_a_real_path__/settings.json");
        let s = Settings::load_or_default(nonexistent);
        assert_eq!(s.hotkey, "Alt+Space");
    }
}
