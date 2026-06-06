-- Initial schema for snipdesk-server. Tables defined here are listed in
-- docs/server-design.md; they're created up front so later phases can
-- wire up endpoints without further migrations on this baseline.
--
-- Conventions:
--   - All timestamps are unix seconds (INTEGER).
--   - All ids are TEXT (UUIDs as strings) for portability and easy debug.
--   - Server-side encrypted columns are BLOB; their nonce + key_version
--     siblings let us rotate keys later without re-encrypting in place.

-- Accounts. Exactly one of password_hash or oidc_subject is populated
-- per row (depending on auth path). is_disabled lets admins lock an
-- account without losing the row's audit value.
CREATE TABLE users (
  id              TEXT PRIMARY KEY,
  email           TEXT NOT NULL UNIQUE,
  display_name    TEXT NOT NULL,
  role            TEXT NOT NULL DEFAULT 'member',
  is_disabled     INTEGER NOT NULL DEFAULT 0,
  created_at      INTEGER NOT NULL,
  last_seen_at    INTEGER,
  password_hash   TEXT,
  oidc_subject    TEXT UNIQUE
);

-- Personal snippets: user-provided content (title, body, tags, folder
-- path) is JSON-serialized then AES-256-GCM-encrypted before insert.
-- The plaintext columns (id, owner_id, version, timestamps) are needed
-- for sync routing and admin visibility (counts, last-modified).
CREATE TABLE personal_snippets (
  id                  TEXT PRIMARY KEY,
  owner_id            TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  payload_ciphertext  BLOB NOT NULL,
  payload_nonce       BLOB NOT NULL,
  key_version         INTEGER NOT NULL,
  version             INTEGER NOT NULL,
  created_at          INTEGER NOT NULL,
  updated_at          INTEGER NOT NULL,
  is_deleted          INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_personal_owner_updated
  ON personal_snippets(owner_id, updated_at);

-- Shared team library: plaintext, admin-managed, visible to every
-- signed-in member. By design these are non-secret (canned replies).
CREATE TABLE library_snippets (
  id              TEXT PRIMARY KEY,
  title           TEXT NOT NULL,
  body            TEXT NOT NULL,
  tags            TEXT NOT NULL DEFAULT '',
  folder_path     TEXT,
  created_by      TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  version         INTEGER NOT NULL
);
CREATE INDEX idx_library_updated ON library_snippets(updated_at);

-- Pre-aggregated per-user activity stats for the admin dashboard. Kept
-- separately from personal_snippets so the dashboard query is O(1) per
-- user instead of a COUNT across thousands of rows.
CREATE TABLE user_activity (
  user_id         TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  snippet_count   INTEGER NOT NULL DEFAULT 0,
  last_sync_at    INTEGER
);
