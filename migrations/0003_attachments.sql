-- Attachments: a record of the backend uses a blob. See DESIGN.md,
-- "Attachments". A blob with an attachment carries no expiry; the first
-- attach clears `blobs.expires_at`, the last detach sets it again.
CREATE TABLE attachments (
    -- v7, the listing cursor.
    id          uuid PRIMARY KEY,
    blob_id     uuid NOT NULL REFERENCES blobs (id) ON DELETE CASCADE,
    -- The kind of record, its identifier as the backend writes it, and
    -- the role the blob plays for it: the Active Storage triple.
    record_type text NOT NULL,
    record_id   text NOT NULL,
    name        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- Attach is idempotent on the triple plus blob; the same index serves
-- every lookup by record, which is why `record_id` is only accepted with
-- `record_type`.
CREATE UNIQUE INDEX attachments_record_idx
    ON attachments (record_type, record_id, name, blob_id);
-- A blob's own attachments, and the last-detach check.
CREATE INDEX attachments_blob_idx ON attachments (blob_id);
-- The low-cardinality scans with the cursor; `record_type` alone through
-- the prefix.
CREATE INDEX attachments_type_name_idx ON attachments (record_type, name, id);

-- Reaper: tombstones to prune, `blobs.keep_purged` after the purge, or
-- after the row went terminal when there was nothing to purge.
CREATE INDEX blobs_purged_at_idx ON blobs (purged_at)
    WHERE purged_at IS NOT NULL;
CREATE INDEX blobs_empty_tombstone_idx ON blobs (updated_at)
    WHERE state IN ('expired', 'deleted')
      AND storage_key IS NULL AND source_parts = 0 AND purged_at IS NULL;
