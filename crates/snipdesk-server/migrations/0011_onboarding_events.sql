-- Onboarding milestones a client can report (e.g. "the user tried the
-- shortcut"). One row per (user, event): the PRIMARY KEY + INSERT OR
-- IGNORE keeps the first occurrence and ignores repeats, so the
-- dashboard funnel counts "users who reached this step" cheaply.
--
-- Only milestones the server can't already infer live here. Steps like
-- "signed up", "saved a snippet", and "first paste" are derived from
-- existing tables (users, personal_snippets, users.snippets_pasted), so
-- the client doesn't report those.

CREATE TABLE onboarding_events (
  user_id  TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  event    TEXT NOT NULL,
  at       INTEGER NOT NULL,
  PRIMARY KEY (user_id, event)
);

CREATE INDEX idx_onboarding_event ON onboarding_events(event);
