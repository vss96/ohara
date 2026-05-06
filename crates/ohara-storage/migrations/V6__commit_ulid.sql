-- Plan 28: per-commit ULID for time-sortable, parallel-write-friendly
-- ordering. Pre-V6 rows get '' (empty default) and are excluded from
-- ULID-ordered reads (e.g. ohara status's MAX(ulid) query) until a
-- --rebuild repopulates them. New writes always include the ULID.
ALTER TABLE commit_record ADD COLUMN ulid TEXT NOT NULL DEFAULT '';
CREATE INDEX idx_commit_record_ulid ON commit_record (ulid);
