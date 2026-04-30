//! Snippet engine: SQLite, settings, Win32 paste, logs, backups, PhraseExpress import.
//!
//! No networking, no Tauri, no async. Network code lives in `snipdesk-teams`;
//! Tauri commands live in `src-tauri`. `shared_library` holds the wire shapes
//! for pulled libraries so core can `replace_team_snippets(...)` without
//! pulling `ureq` into every consumer.

pub mod backup;
pub mod db;
pub mod logging;
pub mod paste;
pub mod phraseexpress;
pub mod settings;
pub mod shared_library;
