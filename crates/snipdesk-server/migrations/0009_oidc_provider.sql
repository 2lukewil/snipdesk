-- Multi-provider OIDC: track which provider issued each oidc_subject.
--
-- Background: the original schema (migration 0001) added
-- `oidc_subject TEXT UNIQUE` to users to track the OIDC subject
-- claim for SSO sign-in. With only Google configured that was
-- sufficient: `sub` is unique within Google's namespace and we
-- never had a second namespace to collide with.
--
-- Adding a second OIDC provider (Keycloak) means `sub` is no
-- longer globally unique. The same `sub` string could in theory be
-- issued by Google for one user and by Keycloak for a completely
-- different user, and the server has no way to tell them apart
-- from the JWT alone. This column records WHICH provider issued
-- each row's sub, so the lookup path can match on
-- `(oidc_provider, oidc_subject)` rather than just `oidc_subject`.
--
-- Design choice: we ADD `oidc_provider` here and LEAVE the existing
-- inline UNIQUE on `oidc_subject` in place. The constraint stays
-- as defence-in-depth - in practice cross-provider sub collisions
-- have probability ~2^-128 (Google IDs are 256-bit, Keycloak UUIDs
-- are 128-bit), so the UNIQUE either never fires or fires as a
-- graceful (if unhelpful) signup-time error. A future migration
-- can drop the global UNIQUE and add a composite
-- `(oidc_provider, oidc_subject)` UNIQUE if that becomes a real
-- problem; SQLite needs a table rebuild for that (no
-- `ALTER TABLE DROP CONSTRAINT`), so it's not worth the migration
-- complexity until the probability becomes nonzero.
--
-- Backfill: every existing OIDC user was Google (the only provider
-- the server supported before this migration), so they get
-- 'google'. Password-only users have `oidc_subject IS NULL` and
-- this column stays NULL too. New rows will set the field
-- explicitly at upsert time.

ALTER TABLE users ADD COLUMN oidc_provider TEXT;

UPDATE users SET oidc_provider = 'google' WHERE oidc_subject IS NOT NULL;
