-- Explicit folder rows for the shared library.
--
-- Until now, folders existed only implicitly: a snippet's
-- `folder_path` value defined the folder, and the sidebar
-- enumerated every distinct path in use. That's fine for "show
-- me where things live," but two features need real folder rows:
--
--   1. Empty folders. The dashboard's new "+ New folder" button
--      creates a folder before any snippets land in it; the
--      sidebar shouldn't drop the row just because the count is
--      zero.
--   2. Manual ordering. Sort by sort_order ASC then path ASC.
--      Default 0 means a fresh folder ties with its siblings,
--      and the path-tiebreak falls through to alphabetical -
--      so a user who never reorders sees identical results to
--      the old behaviour.
--
-- A folder row is created (path, sort_order=0, created_at=now)
-- by either:
--   - the create-folder endpoint (POST /dashboard/library/folders/create)
--   - a snippet save whose folder_path doesn't yet have a row
--     (lazy: keeps the implicit "type a path on save" UX working)
--
-- Folder rename / nest / unnest updates these rows alongside the
-- snippets they cover, so the table never drifts from the set of
-- live folder_path strings.

CREATE TABLE library_folders (
  path        TEXT PRIMARY KEY,
  sort_order  INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL
);

-- Backfill from existing snippet rows so every currently-used
-- folder gets an explicit row at migration time. The
-- "INSERT OR IGNORE" pattern is unnecessary here because the
-- source SELECT is DISTINCT and the target table is empty.
INSERT INTO library_folders (path, sort_order, created_at)
SELECT DISTINCT folder_path,
                0,
                strftime('%s', 'now')
FROM library_snippets
WHERE folder_path IS NOT NULL
  AND folder_path != ''
  AND is_deleted = 0;
