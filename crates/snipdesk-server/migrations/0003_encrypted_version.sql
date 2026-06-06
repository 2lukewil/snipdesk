-- Track which row-version a tombstone's ciphertext was encrypted under.
-- AES-GCM payloads bind their version into the AD; deletes bump
-- personal_snippets.version without re-encrypting, so without this
-- column we have no way to decrypt a tombstoned row at restore time.
-- For existing live rows the encrypted_version equals version (set
-- by the seed UPDATE below). For tombstones it equals (version - 1)
-- because the delete handler bumped version by exactly one without
-- touching the ciphertext.
ALTER TABLE personal_snippets ADD COLUMN encrypted_version INTEGER;

UPDATE personal_snippets
SET encrypted_version = CASE
    WHEN is_deleted = 1 THEN version - 1
    ELSE version
END
WHERE encrypted_version IS NULL;
