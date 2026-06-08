-- Structured audit log for admin mutations.
--
-- The v1 dashboard logs its writes via `tracing::info!` with ad-hoc
-- fields, which is fine for live ops but awful for answering "who
-- promoted Bob, and when?" three months later (you have to grep
-- shipped logs).
--
-- The audit_log table is append-only from the application side. The
-- audit module never UPDATEs or DELETEs rows. Operator-level pruning
-- (e.g. retention) is out of scope - the SQLite file rolls a few
-- megabytes a year at our expected scale, and audit data is exactly
-- the kind of thing you want kept.
--
-- `actor_email` is denormalised so deleting the actor's user row
-- doesn't wipe out their audit trail. `target_id` is intentionally
-- TEXT (no FK) for the same reason - a tombstoned snippet or
-- deleted user's id is still useful evidence even when the parent
-- row is gone.

CREATE TABLE audit_log (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  at           INTEGER NOT NULL,
  -- The user who performed the action. NULL after their account is
  -- deleted, but actor_email keeps the trail readable.
  actor_id     TEXT REFERENCES users(id) ON DELETE SET NULL,
  actor_email  TEXT NOT NULL,
  -- Dotted action codes: "user.create", "user.update", "user.delete",
  -- "library.create", "library.update", "library.delete",
  -- "library.move", "oidc.signin". Easy to grep + extend.
  action       TEXT NOT NULL,
  -- Target kind + id when the action references a specific entity.
  -- NULL for actions that don't target a single row (none today,
  -- but future actions like "config.reload" would use NULL).
  target_kind  TEXT,
  target_id    TEXT,
  -- JSON blob with action-specific detail. For user.update this is
  -- e.g. {"role": {"from": "member", "to": "admin"}}. For library
  -- create/update we record title + folder so a search-by-content
  -- works without re-decrypting (library is plaintext anyway).
  details      TEXT
);

-- Two query patterns the dashboard's audit page wants to be cheap:
--   "most recent N entries"      -> idx_audit_log_at
--   "everything actor X did"     -> idx_audit_log_actor
--   "history of target Y"        -> idx_audit_log_target
CREATE INDEX idx_audit_log_at     ON audit_log(at DESC);
CREATE INDEX idx_audit_log_actor  ON audit_log(actor_id, at DESC);
CREATE INDEX idx_audit_log_target ON audit_log(target_kind, target_id, at DESC);
