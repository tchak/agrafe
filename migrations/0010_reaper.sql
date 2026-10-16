-- The purge records when it removed a blob's objects, so the reaper can
-- tell a terminal blob still holding bytes from one already cleaned up.
ALTER TABLE blobs ADD COLUMN purged_at timestamptz;

-- Reaper: terminal blobs whose objects are still in the bucket.
CREATE INDEX blobs_unpurged_idx ON blobs (id)
    WHERE storage_key IS NOT NULL AND purged_at IS NULL
      AND state IN ('infected', 'expired', 'deleted');

-- Blob-less jobs dedupe too: the periodic `reap` must never pile up, and
-- two processes starting at once both enqueue one. Needs PostgreSQL 15.
DROP INDEX jobs_pending_per_blob_idx;
CREATE UNIQUE INDEX jobs_pending_per_blob_idx ON jobs (kind, blob_id)
    NULLS NOT DISTINCT WHERE locked_at IS NULL;
