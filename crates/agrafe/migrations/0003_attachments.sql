-- Attachments: an owner in the backend uses a blob. See DESIGN.md,
-- "Attachments". A blob with an attachment carries no expiry; the first
-- attach clears `blobs.expires_at`, the last detach sets it again.
CREATE TABLE attachments (
    -- v7, the listing cursor.
    id          uuid PRIMARY KEY,
    blob_id     uuid NOT NULL REFERENCES blobs (id) ON DELETE CASCADE,
    -- The kind of owner, its identifier as the backend writes it, and
    -- the role the blob plays for it.
    owner_kind  text NOT NULL,
    owner_id    text NOT NULL,
    role        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- Attach is idempotent on the triple plus blob; the same index serves
-- every lookup by owner, which is why `owner_id` is only accepted with
-- `owner_kind`.
CREATE UNIQUE INDEX attachments_owner_idx
    ON attachments (owner_kind, owner_id, role, blob_id);
-- A blob's own attachments, and the last-detach check.
CREATE INDEX attachments_blob_idx ON attachments (blob_id);
-- The low-cardinality scans with the cursor.
CREATE INDEX attachments_kind_role_idx ON attachments (owner_kind, role, id);
-- The listing by `owner_kind` alone, with the cursor. The prefix of
-- (owner_kind, role, id) finds the rows but loses the id order after
-- `role`, so each page would sort every attachment of the kind.
CREATE INDEX attachments_kind_idx ON attachments (owner_kind, id);
