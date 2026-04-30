use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Snippet {
    pub id: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    /// "/"-separated. None = root; "" is normalized to None on save.
    #[serde(default)]
    pub folder_path: Option<String>,
    pub usage_count: i64,
    pub last_used: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct NewSnippet {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub folder_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSnippet {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub folder_path: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct FolderInfo {
    /// e.g. "Billing/Refunds".
    pub path: String,
    /// True if at least one snippet is directly in this folder (not just descendants).
    pub has_snippets: bool,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    Alphabetical,
    Usage,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open db {:?}", path))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS snippets (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                tags TEXT NOT NULL DEFAULT '',
                usage_count INTEGER NOT NULL DEFAULT 0,
                last_used INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_snippets_usage ON snippets(usage_count DESC);
            CREATE INDEX IF NOT EXISTS idx_snippets_title ON snippets(title);

            CREATE TABLE IF NOT EXISTS folders (
                path TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS variable_history (
                snippet_id TEXT NOT NULL,
                var_name TEXT NOT NULL,
                value TEXT NOT NULL,
                usage_count INTEGER NOT NULL DEFAULT 1,
                last_used INTEGER NOT NULL,
                PRIMARY KEY (snippet_id, var_name, value)
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS idx_var_hist_lookup
                ON variable_history(snippet_id, var_name, usage_count DESC, last_used DESC);

            -- Team-library snippets fetched from a remote JSON URL. Separate
            -- table so sync can DELETE + bulk reinsert without touching local
            -- snippets. No usage_count — would be misleading on shared data.
            -- Frontend routes through use_snippet via the `team:` id prefix.
            CREATE TABLE IF NOT EXISTS team_snippets (
                team_id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                tags TEXT NOT NULL DEFAULT '',
                folder_path TEXT,
                fetched_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_team_snippets_title ON team_snippets(title);
            CREATE INDEX IF NOT EXISTS idx_team_snippets_folder ON team_snippets(folder_path);",
        )?;

        // Migration: add `folder_path` to snippets if missing.
        let has_folder_col: bool = {
            let mut stmt = conn.prepare("PRAGMA table_info(snippets)")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            let mut found = false;
            for r in rows {
                if let Ok(name) = r {
                    if name == "folder_path" {
                        found = true;
                        break;
                    }
                }
            }
            found
        };
        if !has_folder_col {
            conn.execute("ALTER TABLE snippets ADD COLUMN folder_path TEXT", [])?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_snippets_folder ON snippets(folder_path);",
        )?;

        let me = Db { conn };
        if me.count()? == 0 {
            me.seed_examples()?;
        }
        Ok(me)
    }

    pub fn count(&self) -> Result<i64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM snippets", [], |r| r.get(0))?;
        Ok(n)
    }

    pub fn list(
        &self,
        query: Option<&str>,
        tag: Option<&str>,
        folder: Option<&str>,
        sort: SortOrder,
    ) -> Result<Vec<Snippet>> {
        let mut sql = String::from(
            "SELECT id, title, body, tags, folder_path, usage_count, last_used, created_at, updated_at \
             FROM snippets WHERE 1=1",
        );
        let mut params_vec: Vec<String> = Vec::new();

        if let Some(q) = query.filter(|q| !q.trim().is_empty()) {
            sql.push_str(
                " AND (LOWER(title) LIKE ?1 OR LOWER(body) LIKE ?1 OR LOWER(tags) LIKE ?1)",
            );
            params_vec.push(format!("%{}%", q.to_lowercase()));
        }
        if let Some(t) = tag.filter(|t| !t.trim().is_empty()) {
            let idx = params_vec.len() + 1;
            sql.push_str(&format!(" AND LOWER(tags) LIKE ?{}", idx));
            params_vec.push(format!("%,{},%", t.to_lowercase()));
        }
        // Exact-or-descendant. "__root__" means unfiled.
        if let Some(f) = folder.filter(|f| !f.trim().is_empty()) {
            if f == "__root__" {
                sql.push_str(" AND (folder_path IS NULL OR folder_path = '')");
            } else {
                let eq_idx = params_vec.len() + 1;
                let like_idx = params_vec.len() + 2;
                sql.push_str(&format!(
                    " AND (folder_path = ?{} OR folder_path LIKE ?{})",
                    eq_idx, like_idx
                ));
                params_vec.push(f.to_string());
                params_vec.push(format!("{}/%", f));
            }
        }

        sql.push_str(match sort {
            SortOrder::Usage => " ORDER BY usage_count DESC, updated_at DESC, title ASC",
            SortOrder::Alphabetical => " ORDER BY title COLLATE NOCASE ASC, updated_at DESC",
        });

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(params_vec.iter()),
                row_to_snippet,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get(&self, id: &str) -> Result<Option<Snippet>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, body, tags, folder_path, usage_count, last_used, created_at, updated_at \
             FROM snippets WHERE id = ?1",
        )?;
        let mut rows = stmt.query([id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_snippet(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn create(&self, input: NewSnippet) -> Result<Snippet> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().timestamp();
        let tags = encode_tags(&input.tags);
        let folder = normalize_folder(input.folder_path.as_deref());
        if let Some(f) = folder.as_deref() {
            self.ensure_folder(f)?;
        }
        self.conn.execute(
            "INSERT INTO snippets (id, title, body, tags, folder_path, usage_count, last_used, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0, NULL, ?6, ?6)",
            params![id, input.title, input.body, tags, folder, now],
        )?;
        self.get(&id)?
            .ok_or_else(|| anyhow::anyhow!("created row vanished"))
    }

    pub fn update(&self, id: &str, input: UpdateSnippet) -> Result<Snippet> {
        let now = Utc::now().timestamp();
        let tags = encode_tags(&input.tags);
        let folder = normalize_folder(input.folder_path.as_deref());
        if let Some(f) = folder.as_deref() {
            self.ensure_folder(f)?;
        }
        let n = self.conn.execute(
            "UPDATE snippets SET title = ?1, body = ?2, tags = ?3, folder_path = ?4, updated_at = ?5 WHERE id = ?6",
            params![input.title, input.body, tags, folder, now, id],
        )?;
        if n == 0 {
            anyhow::bail!("snippet not found: {id}");
        }
        self.get(id)?
            .ok_or_else(|| anyhow::anyhow!("snippet vanished after update"))
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        // Cascade variable_history — no FK enforcement.
        self.conn
            .execute("DELETE FROM variable_history WHERE snippet_id = ?1", [id])?;
        let n = self
            .conn
            .execute("DELETE FROM snippets WHERE id = ?1", [id])?;
        if n == 0 {
            anyhow::bail!("snippet not found: {id}");
        }
        Ok(())
    }

    pub fn duplicate(&self, id: &str) -> Result<Snippet> {
        let src = self
            .get(id)?
            .ok_or_else(|| anyhow::anyhow!("snippet not found: {id}"))?;
        let new_title = format!("{} (copy)", src.title);
        self.create(NewSnippet {
            title: new_title,
            body: src.body,
            tags: src.tags,
            folder_path: src.folder_path,
        })
    }

    pub fn record_use(&self, id: &str) -> Result<()> {
        let now = Utc::now().timestamp();
        self.conn.execute(
            "UPDATE snippets SET usage_count = usage_count + 1, last_used = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        Ok(())
    }

    pub fn list_tags(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT tags FROM snippets")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut set = std::collections::BTreeSet::new();
        for r in rows {
            for t in decode_tags(&r?) {
                set.insert(t);
            }
        }
        Ok(set.into_iter().collect())
    }

    // --- Folders ---

    /// Union of explicit rows in `folders` and ancestors implied by
    /// `snippets.folder_path`. Synthesizes missing parents so "Billing/Refunds"
    /// always surfaces "Billing".
    pub fn list_folders(&self) -> Result<Vec<FolderInfo>> {
        let mut paths = std::collections::BTreeSet::new();
        let mut has_direct_snippets = std::collections::BTreeSet::new();

        {
            let mut stmt = self.conn.prepare("SELECT path FROM folders")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for r in rows {
                let p = r?;
                for ancestor in path_ancestors(&p) {
                    paths.insert(ancestor);
                }
            }
        }
        {
            let mut stmt = self
                .conn
                .prepare("SELECT folder_path FROM snippets WHERE folder_path IS NOT NULL AND folder_path <> ''")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for r in rows {
                let p = r?;
                has_direct_snippets.insert(p.clone());
                for ancestor in path_ancestors(&p) {
                    paths.insert(ancestor);
                }
            }
        }

        Ok(paths
            .into_iter()
            .map(|p| {
                let has_snippets = has_direct_snippets.contains(&p);
                FolderInfo {
                    path: p,
                    has_snippets,
                }
            })
            .collect())
    }

    pub fn create_folder(&self, path: &str) -> Result<()> {
        let path = normalize_path(path);
        if path.is_empty() {
            anyhow::bail!("folder path cannot be empty");
        }
        let now = Utc::now().timestamp();
        // Persist each ancestor so empty intermediate folders survive when
        // their only child is renamed or deleted.
        for ancestor in path_ancestors(&path) {
            self.conn.execute(
                "INSERT OR IGNORE INTO folders (path, created_at) VALUES (?1, ?2)",
                params![ancestor, now],
            )?;
        }
        Ok(())
    }

    pub fn rename_folder(&self, old_path: &str, new_path: &str) -> Result<()> {
        let old_path = normalize_path(old_path);
        let new_path = normalize_path(new_path);
        if old_path.is_empty() || new_path.is_empty() {
            anyhow::bail!("folder paths cannot be empty");
        }
        if old_path == new_path {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;

        // Prefix-replace old with new across folders and snippets.
        tx.execute(
            "UPDATE folders SET path = ?1 || SUBSTR(path, ?2) WHERE path = ?3 OR path LIKE ?4",
            params![
                new_path,
                (old_path.len() + 1) as i64, // SUBSTR is 1-indexed
                old_path,
                format!("{}/%", old_path),
            ],
        )?;
        tx.execute(
            "UPDATE snippets SET folder_path = ?1 || SUBSTR(folder_path, ?2) \
             WHERE folder_path = ?3 OR folder_path LIKE ?4",
            params![
                new_path,
                (old_path.len() + 1) as i64,
                old_path,
                format!("{}/%", old_path),
            ],
        )?;

        // Rename may have created a new subtree — ensure its ancestors exist.
        let now = Utc::now().timestamp();
        for ancestor in path_ancestors(&new_path) {
            tx.execute(
                "INSERT OR IGNORE INTO folders (path, created_at) VALUES (?1, ?2)",
                params![ancestor, now],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// `delete_snippets=false` promotes children to root rather than deleting them.
    pub fn delete_folder(&self, path: &str, delete_snippets: bool) -> Result<()> {
        let path = normalize_path(path);
        if path.is_empty() {
            anyhow::bail!("folder path cannot be empty");
        }
        let tx = self.conn.unchecked_transaction()?;
        if delete_snippets {
            tx.execute(
                "DELETE FROM variable_history WHERE snippet_id IN \
                  (SELECT id FROM snippets WHERE folder_path = ?1 OR folder_path LIKE ?2)",
                params![path, format!("{}/%", path)],
            )?;
            tx.execute(
                "DELETE FROM snippets WHERE folder_path = ?1 OR folder_path LIKE ?2",
                params![path, format!("{}/%", path)],
            )?;
        } else {
            tx.execute(
                "UPDATE snippets SET folder_path = NULL \
                 WHERE folder_path = ?1 OR folder_path LIKE ?2",
                params![path, format!("{}/%", path)],
            )?;
        }
        tx.execute(
            "DELETE FROM folders WHERE path = ?1 OR path LIKE ?2",
            params![path, format!("{}/%", path)],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn ensure_folder(&self, path: &str) -> Result<()> {
        let path = normalize_path(path);
        if path.is_empty() {
            return Ok(());
        }
        let now = Utc::now().timestamp();
        for ancestor in path_ancestors(&path) {
            self.conn.execute(
                "INSERT OR IGNORE INTO folders (path, created_at) VALUES (?1, ?2)",
                params![ancestor, now],
            )?;
        }
        Ok(())
    }

    // --- Variable history (autosuggest) ---
    pub fn record_variable_values(
        &self,
        snippet_id: &str,
        vars: &std::collections::HashMap<String, String>,
    ) -> Result<()> {
        if vars.is_empty() {
            return Ok(());
        }
        let now = Utc::now().timestamp();
        let tx = self.conn.unchecked_transaction()?;
        for (name, value) in vars {
            let v = value.trim();
            if v.is_empty() {
                continue;
            }
            tx.execute(
                "INSERT INTO variable_history (snippet_id, var_name, value, usage_count, last_used) \
                 VALUES (?1, ?2, ?3, 1, ?4) \
                 ON CONFLICT(snippet_id, var_name, value) DO UPDATE SET \
                   usage_count = usage_count + 1, last_used = excluded.last_used",
                params![snippet_id, name, v, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_var_history(
        &self,
        snippet_id: &str,
        var_names: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<String>>> {
        let mut out: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        if var_names.is_empty() {
            return Ok(out);
        }
        let mut stmt = self.conn.prepare(
            "SELECT value FROM variable_history \
             WHERE snippet_id = ?1 AND var_name = ?2 \
             ORDER BY usage_count DESC, last_used DESC \
             LIMIT 12",
        )?;
        for name in var_names {
            let values: Vec<String> = stmt
                .query_map(params![snippet_id, name], |r| r.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            out.insert(name.clone(), values);
        }
        Ok(out)
    }

    // --- Export / import ---
    pub fn export_all(&self) -> Result<Vec<Snippet>> {
        self.list(None, None, None, SortOrder::Alphabetical)
    }

    pub fn import(&self, items: Vec<NewSnippet>) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let now = Utc::now().timestamp();
        let mut n = 0;
        for item in items {
            let id = Uuid::new_v4().to_string();
            let tags = encode_tags(&item.tags);
            let folder = normalize_folder(item.folder_path.as_deref());
            if let Some(f) = folder.as_deref() {
                for ancestor in path_ancestors(f) {
                    tx.execute(
                        "INSERT OR IGNORE INTO folders (path, created_at) VALUES (?1, ?2)",
                        params![ancestor, now],
                    )?;
                }
            }
            tx.execute(
                "INSERT INTO snippets (id, title, body, tags, folder_path, usage_count, last_used, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, NULL, ?6, ?6)",
                params![id, item.title, item.body, tags, folder, now],
            )?;
            n += 1;
        }
        tx.commit()?;
        Ok(n)
    }

    fn seed_examples(&self) -> Result<()> {
        let examples = vec![
            NewSnippet {
                title: "Greeting".into(),
                body: "Hi {customer_name},\n\nThanks for reaching out to Shockbyte support! I'm happy to help.".into(),
                tags: vec!["greeting".into(), "general".into()],
                folder_path: Some("General".into()),
            },
            NewSnippet {
                title: "Ask for server details".into(),
                body: "Could you please share your service ID and the exact error message you're seeing? That'll help me track down what's happening on our end.".into(),
                tags: vec!["diagnostic".into()],
                folder_path: Some("Diagnostics".into()),
            },
            NewSnippet {
                title: "Refund follow-up".into(),
                body: "Your refund for invoice #{invoice_id} has been processed. It usually takes 3-5 business days to show up on {payment_method}.\n\nLet me know if you need anything else!".into(),
                tags: vec!["billing".into(), "refund".into()],
                folder_path: Some("Billing/Refunds".into()),
            },
            NewSnippet {
                title: "Cancellation confirmation".into(),
                body: "Your service ({service_type}) has been scheduled for cancellation on {cancellation_date}. You'll have access until that date — after that, the server will be deprovisioned and any data removed.".into(),
                tags: vec!["billing".into(), "cancellation".into()],
                folder_path: Some("Billing/Cancellations".into()),
            },
            NewSnippet {
                title: "Closing".into(),
                body: "Is there anything else I can help you with? Otherwise, have a great day!".into(),
                tags: vec!["closing".into(), "general".into()],
                folder_path: Some("General".into()),
            },
        ];
        self.import(examples)?;
        Ok(())
    }

    /// Case-insensitive title match. `exclude_id` skips self when checking
    /// for conflicts during edit.
    pub fn find_by_title(&self, title: &str, exclude_id: Option<&str>) -> Result<Option<Snippet>> {
        let needle = title.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(None);
        }
        let mut sql = String::from(
            "SELECT id, title, body, tags, folder_path, usage_count, last_used, created_at, updated_at \
             FROM snippets WHERE LOWER(TRIM(title)) = ?1",
        );
        let mut params_vec: Vec<String> = vec![needle];
        if let Some(id) = exclude_id {
            sql.push_str(" AND id <> ?2");
            params_vec.push(id.to_string());
        }
        sql.push_str(" LIMIT 1");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params_vec.iter()))?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_snippet(row)?))
        } else {
            Ok(None)
        }
    }

    // ---- Team library ----

    /// DELETE + bulk insert in one tx. Cheaper than diffing and guarantees
    /// upstream-deleted snippets vanish locally.
    pub fn replace_team_snippets(
        &self,
        snippets: &[crate::shared_library::TeamSnippet],
    ) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM team_snippets", [])?;
        let now = Utc::now().timestamp();
        let mut count = 0;
        for snip in snippets {
            // Fallback id from title — keeps usage history stable across syncs
            // when the author didn't supply one.
            let team_id = snip
                .id
                .clone()
                .unwrap_or_else(|| format!("auto:{}", snip.title.trim().to_lowercase()));
            let tags = encode_tags(&snip.tags);
            let folder = normalize_folder(snip.folder.as_deref());
            tx.execute(
                "INSERT OR REPLACE INTO team_snippets \
                 (team_id, title, body, tags, folder_path, fetched_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![team_id, snip.title, snip.body, tags, folder, now],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }

    /// Surfaces team snippets as `Snippet` with id prefixed `team:<team_id>`
    /// so the frontend renders them through the same path as local ones.
    pub fn list_team_snippets(&self) -> Result<Vec<Snippet>> {
        let mut stmt = self.conn.prepare(
            "SELECT team_id, title, body, tags, folder_path, fetched_at \
             FROM team_snippets ORDER BY title COLLATE NOCASE ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let team_id: String = row.get(0)?;
            let tags: String = row.get(3)?;
            Ok(Snippet {
                id: format!("team:{team_id}"),
                title: row.get(1)?,
                body: row.get(2)?,
                tags: decode_tags(&tags),
                folder_path: row.get(4)?,
                usage_count: 0,
                last_used: None,
                created_at: row.get(5)?,
                updated_at: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Lookup by un-prefixed id. Called by `use_snippet` after stripping `team:`.
    pub fn get_team_snippet(&self, team_id: &str) -> Result<Option<Snippet>> {
        let mut stmt = self.conn.prepare(
            "SELECT team_id, title, body, tags, folder_path, fetched_at \
             FROM team_snippets WHERE team_id = ?1",
        )?;
        let mut rows = stmt.query([team_id])?;
        if let Some(row) = rows.next()? {
            let team_id: String = row.get(0)?;
            let tags: String = row.get(3)?;
            Ok(Some(Snippet {
                id: format!("team:{team_id}"),
                title: row.get(1)?,
                body: row.get(2)?,
                tags: decode_tags(&tags),
                folder_path: row.get(4)?,
                usage_count: 0,
                last_used: None,
                created_at: row.get(5)?,
                updated_at: row.get(5)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn count_team_snippets(&self) -> Result<i64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM team_snippets", [], |r| r.get(0))?;
        Ok(n)
    }
}

fn row_to_snippet(row: &rusqlite::Row) -> rusqlite::Result<Snippet> {
    let tags: String = row.get(3)?;
    let folder: Option<String> = row.get(4)?;
    Ok(Snippet {
        id: row.get(0)?,
        title: row.get(1)?,
        body: row.get(2)?,
        tags: decode_tags(&tags),
        folder_path: folder.filter(|s| !s.is_empty()),
        usage_count: row.get(5)?,
        last_used: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

// Stored as ",tag1,tag2," so LIKE '%,tag,%' works.
fn encode_tags(tags: &[String]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut s = String::from(",");
    for t in tags {
        let t = t.trim().to_lowercase();
        if !t.is_empty() {
            s.push_str(&t);
            s.push(',');
        }
    }
    s
}

fn decode_tags(s: &str) -> Vec<String> {
    s.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn normalize_folder(p: Option<&str>) -> Option<String> {
    let raw = p?;
    let n = normalize_path(raw);
    if n.is_empty() {
        None
    } else {
        Some(n)
    }
}

/// "  Billing // Refunds / " -> "Billing/Refunds".
fn normalize_path(p: &str) -> String {
    p.split('/')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// "Billing/Refunds" -> ["Billing", "Billing/Refunds"].
fn path_ancestors(p: &str) -> Vec<String> {
    let mut out = Vec::new();
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    for i in 1..=parts.len() {
        out.push(parts[..i].join("/"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_strips_whitespace_and_collapses_slashes() {
        assert_eq!(normalize_path("Billing/Refunds"), "Billing/Refunds");
        assert_eq!(normalize_path("  Billing // Refunds / "), "Billing/Refunds");
        assert_eq!(
            normalize_path("/leading/and/trailing/"),
            "leading/and/trailing"
        );
        assert_eq!(normalize_path(""), "");
        assert_eq!(normalize_path("///"), "");
        assert_eq!(normalize_path(" Solo "), "Solo");
    }

    #[test]
    fn path_ancestors_yields_each_prefix_in_order() {
        assert_eq!(
            path_ancestors("Billing/Refunds/Late"),
            vec!["Billing", "Billing/Refunds", "Billing/Refunds/Late"]
        );
        assert_eq!(path_ancestors("Solo"), vec!["Solo".to_string()]);
        let empty: Vec<String> = vec![];
        assert_eq!(path_ancestors(""), empty);
    }
}
