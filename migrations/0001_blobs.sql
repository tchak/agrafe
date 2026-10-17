-- Blobs: one row per stored (or to-be-stored) file. See DESIGN.md, "Blobs".
CREATE TABLE blobs (
    id                uuid PRIMARY KEY,
    state             text NOT NULL
                      CHECK (state IN ('created', 'uploading', 'uploaded', 'ready', 'infected', 'expired', 'deleted')),
    filename          text NOT NULL,
    content_type      text NOT NULL,
    category          text NOT NULL,
    input_type        text NOT NULL
                      CHECK (input_type IN ('raw', 'multipart', 'rows', 'archive')),
    checksum_sha256   bytea CHECK (checksum_sha256 IS NULL OR octet_length(checksum_sha256) = 32),
    size              bigint CHECK (size IS NULL OR size >= 0),
    skip_virus_scan   boolean NOT NULL DEFAULT false,
    expires_at        timestamptz,
    -- Where the blob's jobs go in their kind's queue, as the rank the claim
    -- orders on: 1 high, 0 normal, -1 low. Copied onto every job enqueued
    -- for the blob.
    priority          smallint NOT NULL DEFAULT 0
                      CHECK (priority IN (-1, 0, 1)),

    -- virus scan; scan_signature is what clamd reported for an infected blob
    scan_state        text NOT NULL DEFAULT 'pending'
                      CHECK (scan_state IN ('pending', 'clean', 'infected', 'skipped')),
    scanned_at        timestamptz,
    scan_signature    text,

    -- storage: blobs are write-once, the original lives at `<id>/original`
    storage_key       text,

    -- encryption (envelope: data key wrapped by the KEK named kek_id). The
    -- streaming AEAD header is cached on the row so a range request needs
    -- a single bucket read.
    kek_id            text,
    wrapped_key       bytea,
    encryption_header bytea,
    chunk_size        integer,

    -- derived representations, each with the variant version it was built
    -- with, so a bump can regenerate one without touching the other
    thumbnail_state   text NOT NULL DEFAULT 'pending'
                      CHECK (thumbnail_state IN ('pending', 'ready', 'failed', 'unavailable')),
    preview_state     text NOT NULL DEFAULT 'pending'
                      CHECK (preview_state IN ('pending', 'ready', 'failed', 'unavailable')),
    preview_source    text,
    thumbnail_variant_version integer,
    preview_variant_version   integer,

    -- timestamps. An upload in flight touches upload_heartbeat_at while it
    -- runs, so the reaper can tell a slow upload from one a crashed process
    -- left behind. The purge stamps purged_at once nothing of the blob is
    -- left in the bucket.
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    upload_started_at   timestamptz,
    upload_heartbeat_at timestamptz,
    uploaded_at         timestamptz,
    deleted_at          timestamptz,
    purged_at           timestamptz
);

-- Keep blobs.updated_at current on every UPDATE so no code path has to.
CREATE FUNCTION set_updated_at() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END
$$;

CREATE TRIGGER blobs_updated_at
    BEFORE UPDATE ON blobs
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Listing filters, each paired with id for cursor pagination.
CREATE INDEX blobs_state_idx        ON blobs (state, id);
CREATE INDEX blobs_scan_state_idx   ON blobs (scan_state, id);
CREATE INDEX blobs_category_idx     ON blobs (category, id);
CREATE INDEX blobs_content_type_idx ON blobs (content_type, id);
-- Reaper: expiring blobs.
CREATE INDEX blobs_expires_at_idx   ON blobs (expires_at)
    WHERE expires_at IS NOT NULL AND state NOT IN ('expired', 'deleted');
-- Reaper: stale uploads.
CREATE INDEX blobs_uploading_idx    ON blobs (upload_heartbeat_at)
    WHERE state = 'uploading';
-- Reaper: terminal blobs whose objects are still in the bucket.
CREATE INDEX blobs_unpurged_idx     ON blobs (id)
    WHERE storage_key IS NOT NULL AND purged_at IS NULL
      AND state IN ('infected', 'expired', 'deleted');
