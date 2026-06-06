use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
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
    /// Number of snippets directly in this folder (not counting descendants).
    pub count: i64,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    Alphabetical,
    Usage,
}

#[derive(Debug, Serialize, Clone, Copy)]
pub struct ImportResult {
    pub imported: usize,
    pub skipped_duplicates: usize,
}

/// A locally-edited snippet awaiting push to the server. `server_version`
/// is `None` for rows that have never been synced (the sync engine sends
/// these via POST); `Some(v)` rows go via PUT with `expected_version = v`.
#[derive(Debug, Clone)]
pub struct DirtySnippet {
    pub id: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub folder_path: Option<String>,
    pub server_version: Option<i64>,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open db {path:?}"))?;
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
            -- snippets. No usage_count - would be misleading on shared data.
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
            CREATE INDEX IF NOT EXISTS idx_team_snippets_folder ON team_snippets(folder_path);

            -- Tombstones for snippets the user has deleted locally but
            -- which haven't yet been pushed to the server. The Teams
            -- sync engine drains this table on each tick by issuing
            -- DELETE /api/snippets/:id, then deletes the row here.
            -- Without it we'd lose deletions if the user is offline
            -- when they delete; just removing the row locally leaves
            -- nothing to tell the server about.
            CREATE TABLE IF NOT EXISTS pending_deletes (
                snippet_id  TEXT PRIMARY KEY,
                deleted_at  INTEGER NOT NULL
            );

            -- Generic small-state KV used by the sync engine for the
            -- high-water-mark, the signed-in user descriptor, and the
            -- last error message. Avoids spreading half a dozen one-row
            -- tables across the schema; the values are JSON-encoded when
            -- they're more than a scalar.
            CREATE TABLE IF NOT EXISTS sync_state (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;

        // Migration: add `folder_path` to snippets if missing.
        let has_folder_col: bool = {
            let mut stmt = conn.prepare("PRAGMA table_info(snippets)")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            let mut found = false;
            for name in rows.flatten() {
                if name == "folder_path" {
                    found = true;
                    break;
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

        // Migration: server-sync columns. server_version is the version
        // the server has acknowledged for this row (NULL = never pushed).
        // `dirty` is set whenever the row is created or updated locally;
        // the sync engine resets it after a successful push. Existing
        // rows default to dirty=1 so the first sync after upgrading to
        // Teams uploads everything the user already has.
        let existing_cols: std::collections::HashSet<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(snippets)")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            rows.filter_map(Result::ok).collect()
        };
        if !existing_cols.contains("server_version") {
            conn.execute("ALTER TABLE snippets ADD COLUMN server_version INTEGER", [])?;
        }
        if !existing_cols.contains("dirty") {
            // Default 1 covers both pre-Teams rows (need uploading) and
            // any future inserts (caller can override but most don't).
            conn.execute(
                "ALTER TABLE snippets ADD COLUMN dirty INTEGER NOT NULL DEFAULT 1",
                [],
            )?;
        }
        // Index makes "find dirty rows" cheap on every sync tick.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_snippets_dirty ON snippets(dirty) WHERE dirty = 1;",
        )?;

        // Migration: incremental library-sync columns on team_snippets.
        // `server_version` lets the engine upsert by row instead of the
        // legacy nuke-and-pave (`replace_team_snippets`). Existing local
        // rows from the JSON-URL flow have no server version, so they
        // default to 0 - the next library sync overwrites them in place.
        let team_cols: std::collections::HashSet<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(team_snippets)")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            rows.filter_map(Result::ok).collect()
        };
        if !team_cols.contains("server_version") {
            conn.execute(
                "ALTER TABLE team_snippets ADD COLUMN server_version INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }

        Ok(Db { conn })
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
            sql.push_str(&format!(" AND LOWER(tags) LIKE ?{idx}"));
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
                    " AND (folder_path = ?{eq_idx} OR folder_path LIKE ?{like_idx})"
                ));
                params_vec.push(f.to_string());
                params_vec.push(format!("{f}/%"));
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
        // Mark dirty so the next sync tick pushes the edit. Synced
        // fields are exactly the ones that go into the encrypted payload
        // (title/body/tags/folder_path); usage_count/last_used stay
        // local-only and don't trigger a re-push.
        let n = self.conn.execute(
            "UPDATE snippets \
             SET title = ?1, body = ?2, tags = ?3, folder_path = ?4, updated_at = ?5, dirty = 1 \
             WHERE id = ?6",
            params![input.title, input.body, tags, folder, now, id],
        )?;
        if n == 0 {
            anyhow::bail!("snippet not found: {id}");
        }
        self.get(id)?
            .ok_or_else(|| anyhow::anyhow!("snippet vanished after update"))
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        // Pull the row's server_version (if any) before we drop it, so we
        // know whether the server has heard of this snippet. Rows that
        // were never pushed (server_version IS NULL) can disappear
        // without leaving a tombstone - the server has nothing to delete.
        let server_version: Option<i64> = self
            .conn
            .query_row(
                "SELECT server_version FROM snippets WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        // Cascade variable_history - no FK enforcement.
        self.conn
            .execute("DELETE FROM variable_history WHERE snippet_id = ?1", [id])?;
        let n = self
            .conn
            .execute("DELETE FROM snippets WHERE id = ?1", [id])?;
        if n == 0 {
            anyhow::bail!("snippet not found: {id}");
        }
        // Queue a tombstone only if the server knows about this row.
        // The Teams sync engine drains pending_deletes by issuing
        // DELETE /api/snippets/:id and removing the row here on success.
        if server_version.is_some() {
            let now = Utc::now().timestamp();
            self.conn.execute(
                "INSERT OR REPLACE INTO pending_deletes (snippet_id, deleted_at) VALUES (?1, ?2)",
                params![id, now],
            )?;
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
        // Direct (non-recursive) snippet count per folder_path.
        let mut direct_counts: std::collections::BTreeMap<String, i64> =
            std::collections::BTreeMap::new();

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
            let mut stmt = self.conn.prepare(
                "SELECT folder_path, COUNT(*) FROM snippets \
                 WHERE folder_path IS NOT NULL AND folder_path <> '' \
                 GROUP BY folder_path",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for r in rows {
                let (p, c) = r?;
                direct_counts.insert(p.clone(), c);
                for ancestor in path_ancestors(&p) {
                    paths.insert(ancestor);
                }
            }
        }

        Ok(paths
            .into_iter()
            .map(|p| {
                let count = direct_counts.get(&p).copied().unwrap_or(0);
                FolderInfo {
                    path: p,
                    has_snippets: count > 0,
                    count,
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
        // Snippets touched by the rename get dirty=1 too - folder_path is
        // part of the encrypted payload, so the server needs to be told.
        tx.execute(
            "UPDATE snippets \
             SET folder_path = ?1 || SUBSTR(folder_path, ?2), dirty = 1 \
             WHERE folder_path = ?3 OR folder_path LIKE ?4",
            params![
                new_path,
                (old_path.len() + 1) as i64,
                old_path,
                format!("{}/%", old_path),
            ],
        )?;

        // Rename may have created a new subtree - ensure its ancestors exist.
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
        let like = format!("{path}/%");
        if delete_snippets {
            // Queue tombstones for any deleted snippet the server knows
            // about, BEFORE we drop the rows. Without this, sync would
            // never tell the server about these deletions.
            let now = Utc::now().timestamp();
            tx.execute(
                "INSERT OR REPLACE INTO pending_deletes (snippet_id, deleted_at) \
                 SELECT id, ?3 FROM snippets \
                 WHERE (folder_path = ?1 OR folder_path LIKE ?2) \
                   AND server_version IS NOT NULL",
                params![path, like, now],
            )?;
            tx.execute(
                "DELETE FROM variable_history WHERE snippet_id IN \
                  (SELECT id FROM snippets WHERE folder_path = ?1 OR folder_path LIKE ?2)",
                params![path, like],
            )?;
            tx.execute(
                "DELETE FROM snippets WHERE folder_path = ?1 OR folder_path LIKE ?2",
                params![path, like],
            )?;
        } else {
            // Promote-to-root: folder_path → NULL. Sync-relevant too.
            tx.execute(
                "UPDATE snippets SET folder_path = NULL, dirty = 1 \
                 WHERE folder_path = ?1 OR folder_path LIKE ?2",
                params![path, like],
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

    pub fn import(&self, items: Vec<NewSnippet>) -> Result<ImportResult> {
        // Pre-load existing titles for case-insensitive duplicate detection.
        // Imports that bring the same canned reply twice (a common mistake
        // when exporting+re-importing) would otherwise quietly create
        // hundreds of "Greeting"s in the user's library.
        let mut existing: std::collections::HashSet<String> = self
            .conn
            .prepare("SELECT title FROM snippets")?
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(Result::ok)
            .map(|t| t.trim().to_lowercase())
            .collect();

        let tx = self.conn.unchecked_transaction()?;
        let now = Utc::now().timestamp();
        let mut imported = 0;
        let mut skipped_duplicates = 0;
        for item in items {
            let key = item.title.trim().to_lowercase();
            // Empty titles are unsalvageable; treat as a duplicate of
            // "everything else" rather than letting them in.
            if key.is_empty() || existing.contains(&key) {
                skipped_duplicates += 1;
                continue;
            }
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
            // Track titles seen in this batch too, so a file that itself
            // contains internal duplicates only imports the first copy.
            existing.insert(key);
            imported += 1;
        }
        tx.commit()?;
        Ok(ImportResult {
            imported,
            skipped_duplicates,
        })
    }

    // ---- Server sync primitives ----
    //
    // These methods are the local-DB surface the Teams sync engine
    // (snipdesk-teams::sync) uses to push local edits up and apply
    // remote changes down. The Lite build never calls them but the
    // columns + tables exist regardless, so a user upgrading from Lite
    // to Teams doesn't trigger a schema migration mid-flight.

    /// Rows the user has edited locally that the server hasn't seen yet.
    /// Each tick pushes these and resets `dirty` on success.
    pub fn dirty_snippets(&self) -> Result<Vec<DirtySnippet>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, body, tags, folder_path, server_version \
             FROM snippets WHERE dirty = 1 ORDER BY updated_at ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let tags: String = row.get(3)?;
                let folder: Option<String> = row.get(4)?;
                Ok(DirtySnippet {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    body: row.get(2)?,
                    tags: decode_tags(&tags),
                    folder_path: folder.filter(|s| !s.is_empty()),
                    server_version: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Server acknowledged the push at this version. Clears `dirty` so
    /// the next tick doesn't re-push the same row.
    pub fn mark_synced(&self, id: &str, server_version: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE snippets SET dirty = 0, server_version = ?1 WHERE id = ?2",
            params![server_version, id],
        )?;
        Ok(())
    }

    /// Snippet IDs the user deleted locally that the server still
    /// believes exist (server_version was non-NULL at delete time).
    pub fn pending_deletes(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT snippet_id FROM pending_deletes ORDER BY deleted_at ASC")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(Result::ok)
            .collect();
        Ok(rows)
    }

    /// Server acknowledged the delete; remove the local tombstone.
    pub fn clear_pending_delete(&self, snippet_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM pending_deletes WHERE snippet_id = ?1",
            [snippet_id],
        )?;
        Ok(())
    }

    /// Remote tells us a snippet is gone. Drop the local row without
    /// re-queueing a tombstone (the server has nothing more to learn
    /// about this id).
    pub fn apply_remote_delete(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM variable_history WHERE snippet_id = ?1", [id])?;
        self.conn
            .execute("DELETE FROM snippets WHERE id = ?1", [id])?;
        // Idempotent - also clear any local tombstone for the same id,
        // since the server's already deleted it.
        self.conn
            .execute("DELETE FROM pending_deletes WHERE snippet_id = ?1", [id])?;
        Ok(())
    }

    /// Apply a snippet returned by `GET /api/snippets`. Inserts when
    /// new locally, overwrites when present (last-write-wins per the
    /// design doc; conflict preservation is a v1.1 add). Always clears
    /// `dirty` because the local row now matches what the server has.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_from_remote(
        &self,
        id: &str,
        title: &str,
        body: &str,
        tags: &[String],
        folder_path: Option<&str>,
        server_version: i64,
        created_at: i64,
        updated_at: i64,
    ) -> Result<()> {
        let tags_str = encode_tags(tags);
        let folder = normalize_folder(folder_path);
        if let Some(f) = folder.as_deref() {
            self.ensure_folder(f)?;
        }
        // ON CONFLICT DO UPDATE avoids INSERT OR REPLACE's cascade,
        // which would clobber variable_history. usage_count and
        // last_used stay local-only - we don't overwrite them with
        // server values (which don't have them anyway).
        self.conn.execute(
            "INSERT INTO snippets \
             (id, title, body, tags, folder_path, usage_count, last_used, created_at, updated_at, server_version, dirty) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0, NULL, ?6, ?7, ?8, 0) \
             ON CONFLICT(id) DO UPDATE SET \
               title = excluded.title, \
               body = excluded.body, \
               tags = excluded.tags, \
               folder_path = excluded.folder_path, \
               updated_at = excluded.updated_at, \
               server_version = excluded.server_version, \
               dirty = 0",
            params![id, title, body, tags_str, folder, created_at, updated_at, server_version],
        )?;
        Ok(())
    }

    /// Small KV used by the sync engine (e.g. `high_water_mark`,
    /// `signed_in_user_json`). Returns None when the key isn't set.
    pub fn load_sync_state(&self, key: &str) -> Result<Option<String>> {
        let row = self
            .conn
            .query_row("SELECT value FROM sync_state WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        Ok(row)
    }

    pub fn save_sync_state(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sync_state (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn clear_sync_state(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM sync_state WHERE key = ?1", [key])?;
        Ok(())
    }

    /// Logout housekeeping: drop the high-water-mark and any signed-in
    /// user record, AND reset every snippet's server_version + dirty
    /// state so the next sign-in starts as if from a fresh device.
    /// Local snippet content is preserved - only the sync metadata is
    /// wiped. Pending deletes are also cleared (the new server may not
    /// know about those rows). The library mirror is also wiped - a
    /// different org's shared snippets shouldn't bleed into the next
    /// session, and the new server will repopulate from its own data.
    pub fn reset_sync_metadata(&self) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("UPDATE snippets SET server_version = NULL, dirty = 1", [])?;
        tx.execute("DELETE FROM pending_deletes", [])?;
        tx.execute("DELETE FROM sync_state", [])?;
        tx.execute("DELETE FROM team_snippets", [])?;
        tx.commit()?;
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
            // Fallback id from title - keeps usage history stable across syncs
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

    /// Apply one row from `GET /api/library`. The server pushes both
    /// fresh rows and updates through the same path - `INSERT OR REPLACE`
    /// is fine here because team_snippets has no per-row local state
    /// (no usage_count, no variable_history) that we'd lose.
    pub fn upsert_library_snippet(
        &self,
        team_id: &str,
        title: &str,
        body: &str,
        tags: &[String],
        folder_path: Option<&str>,
        server_version: i64,
    ) -> Result<()> {
        let tags = encode_tags(tags);
        let folder = normalize_folder(folder_path);
        let now = Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO team_snippets \
             (team_id, title, body, tags, folder_path, fetched_at, server_version) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![team_id, title, body, tags, folder, now, server_version],
        )?;
        Ok(())
    }

    /// Remove a library snippet the server says is gone. Mirror of
    /// `apply_remote_delete` on the personal-snippet side.
    pub fn delete_library_snippet(&self, team_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM team_snippets WHERE team_id = ?1", [team_id])?;
        Ok(())
    }

    /// Wipe every library row. Called on logout so a member who signs in
    /// against a different server doesn't carry the previous org's
    /// shared snippets into the new session.
    pub fn clear_library_snippets(&self) -> Result<()> {
        self.conn.execute("DELETE FROM team_snippets", [])?;
        Ok(())
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

    // normalize_path / path_ancestors drive folder hierarchy everywhere
    // (create, rename, ensure_folder, list_folders). Their string handling
    // - slash collapsing, whitespace trim, ancestor expansion - is the kind
    // of logic that breaks silently on a refactor, so it's worth pinning.
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
