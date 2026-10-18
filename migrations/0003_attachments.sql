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
-- The low-cardinality scans with the cursor.
CREATE INDEX attachments_type_name_idx ON attachments (record_type, name, id);
-- The listing by `record_type` alone, with the cursor. The prefix of
-- (record_type, name, id) finds the rows but loses the id order after
-- `name`, so each page would sort every attachment of the type.
CREATE INDEX attachments_type_idx ON attachments (record_type, id);
