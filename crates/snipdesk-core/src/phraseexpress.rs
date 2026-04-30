//! PhraseExpress import.
//!
//! Two formats, schemas vary across versions:
//! - `.pex` — XML. Walk the DOM and grab anything with a description +
//!   phrasecontent (or body/content) pair.
//! - `.pexdb` — SQLite. Introspect tables and pull anything with phrase-like
//!   columns. Modern (v16+) versions encrypt this file by default.
//!
//! Both importers skip on unexpected shape rather than failing the whole import.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use rusqlite::types::ValueRef;
use rusqlite::Connection;

use crate::db::NewSnippet;

/// Dispatch by magic header and extension.
pub fn parse(path: &Path) -> Result<Vec<NewSnippet>> {
    let mut header = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open(path) {
        use std::io::Read;
        let _ = f.read(&mut header);
    }
    if header.starts_with(b"SQLite format 3\0") {
        return parse_pexdb(path);
    }

    // Encrypted .pexdb has high entropy and no magic. Without the PE master
    // key we can't decrypt — bail with a useful error.
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "pexdb" && is_likely_encrypted(&header) {
        return Err(encrypted_pexdb_error());
    }

    parse_pex_xml(path)
}

/// First-16-bytes check: rules out SQLite, XML, BOMs, ASCII tags.
fn is_likely_encrypted(header: &[u8]) -> bool {
    if header.is_empty() {
        return false;
    }
    let first = header[0];
    if first == b'<' || first == b'\r' || first == b'\n' || first == b' ' || first == b'\t' {
        return false;
    }
    if header.starts_with(&[0xEF, 0xBB, 0xBF])
        || header.starts_with(&[0xFF, 0xFE])
        || header.starts_with(&[0xFE, 0xFF])
    {
        return false;
    }
    // Low printable-ratio in 16 bytes is a decent encryption hint.
    let printable = header
        .iter()
        .filter(|&&b| (0x20..=0x7E).contains(&b) || b == b'\r' || b == b'\n' || b == b'\t')
        .count();
    printable < header.len() / 4
}

fn encrypted_pexdb_error() -> anyhow::Error {
    anyhow::anyhow!(
        "This .pexdb file is encrypted (modern PhraseExpress encrypts its database by default) \
         and can't be read directly. In PhraseExpress, go to File → Export → 'Phrase file (*.pex)' \
         and import the resulting .pex XML file here instead."
    )
}

// ---------------- .pex XML ----------------

pub fn parse_pex_xml(path: &Path) -> Result<Vec<NewSnippet>> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("read pex xml file {:?}", path))?;

    let mut reader = Reader::from_str(&contents);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut folder_stack: Vec<String> = Vec::new();
    let mut current_element: Option<String> = None;
    let mut current_phrase: HashMap<String, String> = HashMap::new();
    // None if not currently inside a phrase element.
    let mut phrase_depth: Option<usize> = None;
    let mut depth: usize = 0;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                depth += 1;
                let name = e.name();
                let tag = std::str::from_utf8(name.as_ref())
                    .unwrap_or("")
                    .to_lowercase();

                if phrase_depth.is_none() && is_phrase_tag(&tag) {
                    phrase_depth = Some(depth);
                    current_phrase.clear();
                    // Some versions store content in attrs rather than children.
                    for a in e.attributes().flatten() {
                        let key = std::str::from_utf8(a.key.as_ref())
                            .unwrap_or("")
                            .to_lowercase();
                        if let Ok(val) = a.unescape_value() {
                            current_phrase.insert(key, val.into_owned());
                        }
                    }
                } else if phrase_depth.is_none() && is_folder_tag(&tag) {
                    let mut folder_name = String::new();
                    for a in e.attributes().flatten() {
                        let key = std::str::from_utf8(a.key.as_ref())
                            .unwrap_or("")
                            .to_lowercase();
                        if matches!(key.as_str(), "description" | "name" | "title") {
                            if let Ok(v) = a.unescape_value() {
                                folder_name = v.into_owned();
                            }
                        }
                    }
                    folder_stack.push(folder_name);
                }

                if phrase_depth.is_some() {
                    current_element = Some(tag);
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(field) = &current_element {
                    if phrase_depth.is_some() {
                        if let Ok(txt) = t.unescape() {
                            let entry = current_phrase.entry(field.clone()).or_default();
                            if !entry.is_empty() {
                                entry.push(' ');
                            }
                            entry.push_str(&txt);
                        }
                    }
                }
            }
            Ok(Event::CData(t)) => {
                if let Some(field) = &current_element {
                    if phrase_depth.is_some() {
                        if let Ok(bytes) = std::str::from_utf8(t.as_ref()) {
                            let entry = current_phrase.entry(field.clone()).or_default();
                            if !entry.is_empty() {
                                entry.push(' ');
                            }
                            entry.push_str(bytes);
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let tag = std::str::from_utf8(name.as_ref())
                    .unwrap_or("")
                    .to_lowercase();

                if phrase_depth == Some(depth) && is_phrase_tag(&tag) {
                    if let Some(sn) = phrase_from_map(&current_phrase, &folder_stack) {
                        out.push(sn);
                    }
                    current_phrase.clear();
                    phrase_depth = None;
                    current_element = None;
                } else if phrase_depth.is_none() && is_folder_tag(&tag) {
                    folder_stack.pop();
                } else {
                    current_element = None;
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "xml parse error at pos {}: {}",
                    reader.buffer_position(),
                    e
                ));
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(out)
}

fn is_phrase_tag(tag: &str) -> bool {
    matches!(
        tag,
        "phrase" | "pxphrase" | "px_phrase" | "phraseentry" | "entry"
    )
}

fn is_folder_tag(tag: &str) -> bool {
    matches!(tag, "folder" | "pxfolder" | "px_folder" | "group")
}

fn phrase_from_map(m: &HashMap<String, String>, folders: &[String]) -> Option<NewSnippet> {
    // Field aliases vary across PhraseExpress versions.
    let title = first_nonempty(m, &["description", "name", "title", "shortname", "subject"]);
    let body = first_nonempty(
        m,
        &[
            "phrasecontent",
            "phrase",
            "content",
            "body",
            "text",
            "value",
        ],
    );
    let autotext = first_nonempty(m, &["autotext", "shortcut", "abbreviation", "key"]);

    let body = body?;
    let body = cleanup_body(&body);
    if body.trim().is_empty() {
        return None;
    }

    let title = title
        .or_else(|| autotext.clone())
        .unwrap_or_else(|| summarize(&body));

    // Folder chain → folder_path. Autotext stays as a `shortcut:` tag so
    // abbreviation search still works.
    let folder_path = {
        let parts: Vec<String> = folders
            .iter()
            .filter(|f| !f.trim().is_empty())
            .map(|f| f.trim().to_string())
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("/"))
        }
    };
    let mut tags: Vec<String> = Vec::new();
    if let Some(a) = autotext {
        if !a.trim().is_empty() {
            tags.push(format!("shortcut:{}", a.trim().to_lowercase()));
        }
    }

    Some(NewSnippet {
        title,
        body,
        tags,
        folder_path,
    })
}

fn first_nonempty(m: &HashMap<String, String>, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(v) = m.get(*k) {
            if !v.trim().is_empty() {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn summarize(body: &str) -> String {
    let line = body.lines().next().unwrap_or(body);
    let trimmed = line.trim();
    if trimmed.len() <= 60 {
        trimmed.to_string()
    } else {
        format!("{}…", &trimmed[..60])
    }
}

// ---------------- .pexdb SQLite ----------------

pub fn parse_pexdb(path: &Path) -> Result<Vec<NewSnippet>> {
    // Double-check the magic — encrypted .pexdb doesn't start with "SQLite format 3\0".
    use std::io::Read;
    let mut header = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open(path) {
        let _ = f.read(&mut header);
    }
    if !header.starts_with(b"SQLite format 3\0") {
        return Err(encrypted_pexdb_error());
    }

    // PE may have the file locked or have a stray WAL — work from a temp copy.
    let temp_path =
        std::env::temp_dir().join(format!("snipdesk-import-{}.pexdb", std::process::id()));
    std::fs::copy(path, &temp_path)
        .with_context(|| format!("copy pexdb to temp {:?}", temp_path))?;

    // immutable=1 stops SQLite trying to create journals.
    let uri = format!(
        "file:{}?mode=ro&immutable=1",
        temp_path.to_string_lossy().replace('\\', "/")
    );
    let conn = rusqlite::Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open pexdb {:?}", temp_path))?;

    let result = parse_pexdb_conn(&conn);
    drop(conn);
    let _ = std::fs::remove_file(&temp_path);
    let tables = result?;
    Ok(tables)
}

/// Split out so `parse_pexdb` can always clean up the temp file.
fn parse_pexdb_conn(conn: &Connection) -> Result<Vec<NewSnippet>> {
    let tables = list_tables(conn)?;
    let folder_names = build_folder_map(conn, &tables).unwrap_or_default();

    let mut out = Vec::new();
    let mut tables_scanned = 0usize;
    let mut rows_scanned = 0usize;
    let mut rows_decoded = 0usize;

    for table in &tables {
        let cols = list_columns(conn, table)?;
        let col_lower: Vec<String> = cols.iter().map(|c| c.to_lowercase()).collect();

        let body_col = pick_col(
            &col_lower,
            &cols,
            &[
                "phrasecontent",
                "phrase",
                "content",
                "body",
                "text",
                "value",
                "phrase_text",
                "rtf",
                "rtfcontent",
                "html",
                "htmlcontent",
                "data",
            ],
        );
        let title_col = pick_col(
            &col_lower,
            &cols,
            &["description", "name", "title", "shortname", "subject"],
        );
        let autotext_col = pick_col(
            &col_lower,
            &cols,
            &["autotext", "shortcut", "abbreviation", "key"],
        );
        let parent_col = pick_col(
            &col_lower,
            &cols,
            &["parent_id", "parentid", "folder_id", "folderid", "parent"],
        );

        let Some(body_col) = body_col else {
            continue; // not a phrase-like table
        };
        tables_scanned += 1;

        let mut select_cols = vec![body_col.clone()];
        let title_idx = title_col.as_ref().map(|c| {
            select_cols.push(c.clone());
            select_cols.len() - 1
        });
        let autotext_idx = autotext_col.as_ref().map(|c| {
            select_cols.push(c.clone());
            select_cols.len() - 1
        });
        let parent_idx = parent_col.as_ref().map(|c| {
            select_cols.push(c.clone());
            select_cols.len() - 1
        });

        let sql = format!(
            "SELECT {} FROM \"{}\"",
            select_cols
                .iter()
                .map(|c| format!("\"{}\"", c))
                .collect::<Vec<_>>()
                .join(", "),
            table
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("pexdb: skipping table {table}: {err}");
                continue;
            }
        };

        // Bodies are TEXT or BLOB (RTF / UTF-16LE / UTF-8) depending on version.
        let rows = stmt.query_map([], |row| {
            let body = decode_cell(row.get_ref(0)?);
            let title = title_idx.and_then(|i| row.get_ref(i).ok().and_then(decode_cell));
            let autotext = autotext_idx.and_then(|i| row.get_ref(i).ok().and_then(decode_cell));
            let parent = parent_idx.and_then(|i| match row.get_ref(i).ok()? {
                ValueRef::Integer(n) => Some(n),
                _ => None,
            });
            Ok((body, title, autotext, parent))
        });

        let Ok(rows) = rows else { continue };
        for row in rows.flatten() {
            rows_scanned += 1;
            let (body, title, autotext, parent) = row;
            let Some(body) = body else { continue };
            let body = cleanup_body(&body);
            if body.trim().is_empty() {
                continue;
            }
            rows_decoded += 1;

            let mut tags: Vec<String> = Vec::new();
            let folder_path = parent.and_then(|pid| {
                folder_names
                    .get(&pid)
                    .map(|f| f.trim().to_string())
                    .filter(|f| !f.is_empty())
            });
            if let Some(a) = autotext.as_ref() {
                if !a.trim().is_empty() {
                    tags.push(format!("shortcut:{}", a.trim().to_lowercase()));
                }
            }

            let title = title
                .filter(|t| !t.trim().is_empty())
                .or_else(|| autotext.clone().filter(|t| !t.trim().is_empty()))
                .unwrap_or_else(|| summarize(&body));

            out.push(NewSnippet {
                title: title.trim().to_string(),
                body,
                tags,
                folder_path,
            });
        }
    }

    eprintln!(
        "pexdb: scanned {} phrase-like tables, decoded {}/{} rows into snippets",
        tables_scanned, rows_decoded, rows_scanned
    );

    Ok(out)
}

/// Handles the three shapes PE ships: UTF-16LE blob (with or without BOM),
/// RTF blob, straight UTF-8.
fn decode_cell(v: ValueRef<'_>) -> Option<String> {
    match v {
        ValueRef::Null => None,
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => decode_bytes(bytes),
        ValueRef::Integer(n) => Some(n.to_string()),
        ValueRef::Real(n) => Some(n.to_string()),
    }
}

fn decode_bytes(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }

    // UTF-16LE BOM.
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        return decode_utf16le(&bytes[2..]);
    }
    // UTF-16BE BOM — rare but seen.
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let u16s: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16(&u16s).ok();
    }
    let rest = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &bytes[3..]
    } else {
        bytes
    };

    // BOM-less UTF-16: detect by zero distribution.
    if rest.len() >= 2 && rest.len() % 2 == 0 {
        let zeros_odd = rest.iter().skip(1).step_by(2).filter(|b| **b == 0).count();
        let zeros_even = rest.iter().step_by(2).filter(|b| **b == 0).count();
        let half = rest.len() / 4;
        if zeros_odd > half && zeros_odd > zeros_even {
            if let Some(s) = decode_utf16le(rest) {
                return Some(s);
            }
        }
    }

    if let Ok(s) = std::str::from_utf8(rest) {
        return Some(s.to_string());
    }

    // Windows-1252 last resort — every byte maps to a valid codepoint.
    Some(decode_windows_1252(rest))
}

fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    if bytes.len() % 2 != 0 {
        return None;
    }
    let u16s: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&u16s).ok()
}

/// Windows-1252 → Unicode. Only 0x80–0x9F differs from ISO-8859-1.
fn decode_windows_1252(bytes: &[u8]) -> String {
    const C1: [char; 32] = [
        '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}',
        '\u{017D}', '\u{FFFD}', '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
    ];
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            0x80..=0x9F => out.push(C1[(b - 0x80) as usize]),
            _ => out.push(b as char),
        }
    }
    out
}

/// Strip RTF / HTML wrappers down to plain text.
fn cleanup_body(s: &str) -> String {
    let trimmed = s.trim_start_matches('\u{FEFF}').trim();
    if trimmed.starts_with("{\\rtf") || trimmed.starts_with(r"{\rtf") {
        return rtf_to_plain(trimmed);
    }
    let lower_head: String = trimmed.chars().take(200).collect::<String>().to_lowercase();
    if lower_head.contains("<html")
        || lower_head.contains("<!doctype html")
        || lower_head.starts_with("<p>")
    {
        return strip_html(trimmed);
    }
    trimmed.to_string()
}

/// Handles `\par` / `\line` / `\tab` / `\'hh` / `\uNNNN?` plus group skipping
/// for fonttbl/colortbl/stylesheet/pict/etc. Not a real RTF parser.
fn rtf_to_plain(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    let mut skip_depth: Option<usize> = None;
    let mut depth: usize = 0;

    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                if let Some(d) = skip_depth {
                    if depth <= d {
                        skip_depth = None;
                    }
                }
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'\\' if i + 1 < bytes.len() => {
                // Escaped \, {, }.
                if bytes[i + 1] == b'\\' || bytes[i + 1] == b'{' || bytes[i + 1] == b'}' {
                    if skip_depth.is_none() {
                        out.push(bytes[i + 1] as char);
                    }
                    i += 2;
                    continue;
                }
                if bytes[i + 1] == b'\'' && i + 3 < bytes.len() {
                    // \'hh — codepage byte. PE always emits Windows-1252.
                    if bytes[i + 2].is_ascii_hexdigit() && bytes[i + 3].is_ascii_hexdigit() {
                        if let Ok(hex) = std::str::from_utf8(&bytes[i + 2..i + 4]) {
                            if let Ok(b) = u8::from_str_radix(hex, 16) {
                                if skip_depth.is_none() {
                                    out.push_str(&decode_windows_1252(&[b]));
                                }
                            }
                        }
                    }
                    i += 4;
                    continue;
                }
                // \name or \nameNNN
                let word_start = i + 1;
                let mut j = word_start;
                while j < bytes.len() && (bytes[j].is_ascii_alphabetic()) {
                    j += 1;
                }
                let word = &src[word_start..j];
                let num_start = j;
                if j < bytes.len() && (bytes[j] == b'-' || bytes[j].is_ascii_digit()) {
                    j += 1;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                }
                let num: Option<i32> = if j > num_start {
                    src[num_start..j].parse().ok()
                } else {
                    None
                };
                // Trailing space delimits the control word — consume it.
                if j < bytes.len() && bytes[j] == b' ' {
                    j += 1;
                }

                match word {
                    "par" | "line" | "sect" | "page" => {
                        if skip_depth.is_none() {
                            out.push('\n');
                        }
                    }
                    "tab" => {
                        if skip_depth.is_none() {
                            out.push('\t');
                        }
                    }
                    "u" => {
                        if let Some(n) = num {
                            let code = if n < 0 {
                                (n + 0x10000) as u32
                            } else {
                                n as u32
                            };
                            if let Some(ch) = char::from_u32(code) {
                                if skip_depth.is_none() {
                                    out.push(ch);
                                }
                            }
                            // \uNNNN is usually followed by a single fallback char.
                            if j < bytes.len() && bytes[j] == b'?' {
                                j += 1;
                            }
                        }
                    }
                    "fonttbl" | "colortbl" | "stylesheet" | "pict" | "object" | "info"
                    | "generator" | "themedata" | "colorschememapping" | "latentstyles"
                    | "listtable" | "revtbl" | "rsidtbl" | "filetbl" | "xmlnstbl" => {
                        if skip_depth.is_none() {
                            skip_depth = Some(depth);
                        }
                    }
                    _ => {}
                }
                i = j;
            }
            b'\r' | b'\n' => i += 1, // raw newlines aren't content in RTF
            _ => {
                if skip_depth.is_none() {
                    out.push(c as char);
                }
                i += 1;
            }
        }
    }

    out.split('\n')
        .map(|l| l.trim_end_matches(' ').to_string())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn strip_html(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut in_tag = false;
    for c in src.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    out.trim().to_string()
}

fn list_tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn list_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let sql = format!("PRAGMA table_info(\"{}\")", table);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn pick_col(lower: &[String], original: &[String], candidates: &[&str]) -> Option<String> {
    for c in candidates {
        if let Some(i) = lower.iter().position(|x| x == c) {
            return Some(original[i].clone());
        }
    }
    None
}

fn build_folder_map(conn: &Connection, tables: &[String]) -> Result<HashMap<i64, String>> {
    let mut map = HashMap::new();
    for table in tables {
        let cols = list_columns(conn, table)?;
        let lower: Vec<String> = cols.iter().map(|c| c.to_lowercase()).collect();

        let id_col = pick_col(&lower, &cols, &["id", "folder_id", "folderid"]);
        let name_col = pick_col(&lower, &cols, &["description", "name", "title"]);
        let body_col = pick_col(
            &lower,
            &cols,
            &[
                "phrasecontent",
                "phrase",
                "content",
                "body",
                "text",
                "phrase_text",
            ],
        );

        // Folder-shaped: id + name, no body column.
        if let (Some(id), Some(name)) = (id_col, name_col) {
            if body_col.is_none() {
                let sql = format!("SELECT \"{}\", \"{}\" FROM \"{}\"", id, name, table);
                if let Ok(mut stmt) = conn.prepare(&sql) {
                    if let Ok(iter) = stmt.query_map([], |r| {
                        let id = r.get::<_, i64>(0)?;
                        let name = decode_cell(r.get_ref(1)?).unwrap_or_default();
                        Ok((id, name))
                    }) {
                        for row in iter.flatten() {
                            map.insert(row.0, row.1);
                        }
                    }
                }
            }
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_likely_encrypted_recognizes_xml_starts() {
        assert!(!is_likely_encrypted(b"<?xml version=\"1.0\""));
        assert!(!is_likely_encrypted(b"<phrasebook>"));
        assert!(!is_likely_encrypted(b"\r\n<root>"));
        assert!(!is_likely_encrypted(b"  <padded>"));
    }

    #[test]
    fn is_likely_encrypted_skips_known_boms() {
        assert!(!is_likely_encrypted(&[0xEF, 0xBB, 0xBF, b'<', b'?']));
        assert!(!is_likely_encrypted(&[0xFF, 0xFE, b'<', 0, b'?', 0]));
        assert!(!is_likely_encrypted(&[0xFE, 0xFF, 0, b'<', 0, b'?']));
    }

    #[test]
    fn is_likely_encrypted_flags_high_entropy_payload() {
        // 16 high-entropy bytes — almost certainly ciphertext, not text.
        let mut buf = [0u8; 16];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((i as u8).wrapping_mul(173)).wrapping_add(0xA5);
        }
        assert!(is_likely_encrypted(&buf));
    }

    #[test]
    fn is_likely_encrypted_handles_empty() {
        assert!(!is_likely_encrypted(&[]));
    }

    #[test]
    fn is_likely_encrypted_passes_plain_ascii() {
        assert!(!is_likely_encrypted(b"INSERT INTO phrases VALUES"));
        assert!(!is_likely_encrypted(b"SQLite format 3\0"));
    }
}
