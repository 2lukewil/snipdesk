//! File logging + panic capture. Windowed builds have no stderr, so panics
//! went into the void without this. Log lives at
//! `%APPDATA%\SnipDesk\logs\snipdesk.log`; wiped on startup if older than
//! `retention_days`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use once_cell::sync::OnceCell;

// Mutex'd because panic hook + background threads both write.
static LOG_FILE: OnceCell<Mutex<Option<File>>> = OnceCell::new();
static LOG_PATH: OnceCell<PathBuf> = OnceCell::new();

/// Call once early in setup(). Wipes the log if older than `retention_days`,
/// opens append, installs a panic hook.
pub fn init(data_dir: &Path, retention_days: u32) {
    let logs_dir = data_dir.join("logs");
    if let Err(err) = std::fs::create_dir_all(&logs_dir) {
        eprintln!("logging: failed to create logs dir: {err}");
        return;
    }
    let log_path = logs_dir.join("snipdesk.log");

    // Wipe before open so the truncate is a single step.
    let retention = Duration::from_secs(retention_days as u64 * 24 * 60 * 60);
    if let Ok(meta) = std::fs::metadata(&log_path) {
        if let Ok(modified) = meta.modified() {
            let age = SystemTime::now()
                .duration_since(modified)
                .unwrap_or(Duration::ZERO);
            if age > retention {
                let _ = std::fs::remove_file(&log_path);
            }
        }
    }

    let file = match OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(f) => f,
        Err(err) => {
            eprintln!(
                "logging: failed to open log file {}: {err}",
                log_path.display()
            );
            return;
        }
    };

    let _ = LOG_FILE.set(Mutex::new(Some(file)));
    let _ = LOG_PATH.set(log_path.clone());

    // Session header makes grep-by-restart easy.
    log_raw(&format!(
        "=== SnipDesk session started at {} ===",
        now_iso8601()
    ));

    // Chain to the default hook so console output still works under `cargo run`.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        log_raw(&format!("PANIC @ {}: {}", location, payload));
        default_hook(info);
    }));
}

/// `YYYY-MM-DDTHH:MM:SS ERROR <msg>`. Also echoes to stderr.
pub fn log_error(msg: &str) {
    let line = format!("{} ERROR {}", now_iso8601(), msg);
    eprintln!("{line}");
    log_raw(&line);
}

pub fn log_info(msg: &str) {
    let line = format!("{} INFO  {}", now_iso8601(), msg);
    log_raw(&line);
}

// No-op if init() didn't run or the file failed to open. Logging must never panic.
fn log_raw(line: &str) {
    let Some(mutex) = LOG_FILE.get() else {
        return;
    };
    let Ok(mut guard) = mutex.lock() else {
        return;
    };
    if let Some(file) = guard.as_mut() {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

fn now_iso8601() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// Used by the "View logs" button in the frontend.
pub fn log_path() -> Option<PathBuf> {
    LOG_PATH.get().cloned()
}
