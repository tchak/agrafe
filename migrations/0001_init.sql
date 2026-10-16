-- Blobs: one row per stored (or to-be-stored) file.
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
    write_once        boolean NOT NULL DEFAULT true,
    skip_virus_scan   boolean NOT NULL DEFAULT false,
    expires_at        timestamptz,

    -- virus scan
    scan_state        text NOT NULL DEFAULT 'pending'
                      CHECK (scan_state IN ('pending', 'clean', 'infected', 'skipped')),
    scanned_at        timestamptz,

    -- storage
    storage_key       text,
    version           integer NOT NULL DEFAULT 0,

    -- encryption (envelope: data key wrapped by the KEK named kek_id)
    kek_id            text,
    wrapped_key       bytea,
    nonce_prefix      bytea,
    chunk_size        integer,

    -- derived representations
    thumbnail_state   text NOT NULL DEFAULT 'pending'
                      CHECK (thumbnail_state IN ('pending', 'ready', 'failed', 'unavailable')),
    preview_state     text NOT NULL DEFAULT 'pending'
                      CHECK (preview_state IN ('pending', 'ready', 'failed', 'unavailable')),
    preview_source    text,
    variant_version   integer,

    -- timestamps
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    upload_started_at timestamptz,
    uploaded_at       timestamptz,
    deleted_at        timestamptz
);

-- Listing filters, each paired with id for cursor pagination.
CREATE INDEX blobs_state_idx        ON blobs (state, id);
CREATE INDEX blobs_scan_state_idx   ON blobs (scan_state, id);
CREATE INDEX blobs_category_idx     ON blobs (category, id);
CREATE INDEX blobs_content_type_idx ON blobs (content_type, id);
-- Reaper: expiring blobs.
CREATE INDEX blobs_expires_at_idx   ON blobs (expires_at)
    WHERE expires_at IS NOT NULL AND state NOT IN ('expired', 'deleted');
-- Reaper: stale uploads.
CREATE INDEX blobs_uploading_idx    ON blobs (upload_started_at)
    WHERE state = 'uploading';

-- Jobs: the Postgres-backed queue.
CREATE TABLE jobs (
    id           bigserial PRIMARY KEY,
    kind         text NOT NULL,
    blob_id      uuid REFERENCES blobs (id) ON DELETE CASCADE,
    payload      jsonb NOT NULL DEFAULT '{}'::jsonb,
    run_at       timestamptz NOT NULL DEFAULT now(),
    attempts     integer NOT NULL DEFAULT 0,
    max_attempts integer NOT NULL DEFAULT 5,
    locked_at    timestamptz,
    locked_by    text,
    last_error   text,
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX jobs_ready_idx ON jobs (run_at) WHERE locked_at IS NULL;
-- One pending job of each kind per blob. Enqueue uses ON CONFLICT DO NOTHING.
CREATE UNIQUE INDEX jobs_pending_per_blob_idx ON jobs (kind, blob_id) WHERE locked_at IS NULL;
