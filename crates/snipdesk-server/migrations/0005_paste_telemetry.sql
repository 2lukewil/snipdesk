-- Paste telemetry. The desktop client tracks usage_count and last_used
-- locally per snippet; this migration adds the server-side aggregates
-- so the admin dashboard can compute hours/money saved from real data
-- (instead of the inventory-size proxy that shipped in 0004).
--
-- Deltas are pushed by the client via POST /api/usage/report on each
-- sync tick. The server applies them idempotently:
--   - users.chars_pasted += delta              (running total)
--   - users.snippets_pasted += paste_count     (event count)
--   - personal_snippets.usage_count += delta   (owner-scoped)
--   - library_usage row upserted per (user, library_snippet)
--
-- Per-user wpm/hourly_wage/currency are nullable overrides. NULL means
-- "use the [stats] block from the server config"; non-NULL lets each
-- user dial in their own number from the desktop settings UI. The
-- dashboard's per-user money/time estimates pick the right value for
-- each person.

ALTER TABLE users ADD COLUMN chars_pasted INTEGER NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN snippets_pasted INTEGER NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN wpm INTEGER;
ALTER TABLE users ADD COLUMN hourly_wage REAL;
ALTER TABLE users ADD COLUMN currency TEXT;

-- Per-personal-snippet usage. Kept outside the encrypted payload so the
-- dashboard can sort by "most used" without decrypting. Scoped to the
-- owner via personal_snippets.owner_id (so the same id from another
-- user can't collide; PK on personal_snippets is already global).
ALTER TABLE personal_snippets ADD COLUMN usage_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE personal_snippets ADD COLUMN last_used INTEGER;
CREATE INDEX idx_personal_owner_usage
  ON personal_snippets(owner_id, usage_count DESC);

-- Per-(user, library snippet) usage. Library snippets are shared, so
-- the counter has to be keyed by user too - otherwise we couldn't
-- tell which users actually use the snippet, only the team total.
-- snippet_id is intentionally NOT a foreign key: we want this row to
-- survive library_snippets purges so historical activity stays
-- attributable. The aggregate views handle missing parent rows
-- gracefully.
CREATE TABLE library_usage (
  user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  snippet_id   TEXT NOT NULL,
  usage_count  INTEGER NOT NULL DEFAULT 0,
  last_used    INTEGER NOT NULL,
  PRIMARY KEY (user_id, snippet_id)
);
CREATE INDEX idx_library_usage_snippet
  ON library_usage(snippet_id, last_used);
CREATE INDEX idx_library_usage_user_last
  ON library_usage(user_id, last_used);
