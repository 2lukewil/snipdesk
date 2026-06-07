-- Sync invariants: the per-user version counter (personal_snippets)
-- and the org-wide library version counter must be strictly monotonic.
-- The handler computes `MAX(version) + 1` and races with itself under
-- concurrent writers; without these unique indexes two simultaneous
-- creates could both produce version=N, breaking the `since` cursor
-- (clients would see snippet B at N and skip snippet A because their
-- HWM advanced past it). The unique constraint makes the loser fail
-- at insert time, surfaced as a 409 the client retries.
--
-- Also adds the index sync queries actually need - `WHERE owner_id =
-- ? AND version > ? ORDER BY version ASC` was scanning all of a
-- user's rows because the existing `idx_personal_owner_updated` keys
-- on updated_at, not version.

CREATE UNIQUE INDEX IF NOT EXISTS idx_personal_owner_version
  ON personal_snippets(owner_id, version);

CREATE UNIQUE INDEX IF NOT EXISTS idx_library_version_unique
  ON library_snippets(version);
