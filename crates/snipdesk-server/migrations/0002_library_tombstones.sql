-- Phase 5: shared library gets tombstones so the sync stream can tell
-- clients "this id is gone" the same way personal snippets do. Existing
-- rows are alive by definition; default 0 covers them.
ALTER TABLE library_snippets ADD COLUMN is_deleted INTEGER NOT NULL DEFAULT 0;

-- Sync queries `WHERE version > since ORDER BY version ASC` - version is
-- the monotonic clock for library updates, identical in spirit to the
-- personal_snippets index added in 0001.
CREATE INDEX IF NOT EXISTS idx_library_version ON library_snippets(version);
