-- Blobs: one row per stored (or to-be-stored) file. See DESIGN.md, "Blobs".
CREATE TABLE blobs (
    id                uuid PRIMARY KEY,
    state             text NOT NULL
                      CHECK (state IN ('created', 'uploading', 'uploaded', 'ready', 'infected', 'expired', 'deleted')),
    filename          text NOT NULL,
    content_type      text NOT NULL,
    category          text NOT NULL
                      CHECK (category IN ('image', 'document', 'spreadsheet', 'presentation', 'archive', 'audio', 'video', 'text', 'other')),
    input_type        text NOT NULL
                      CHECK (input_type IN ('raw', 'multipart', 'url', 'rows', 'archive', 'document')),
    -- Declared at creation for raw and multipart, optionally for url, and
    -- written by the upload or the build otherwise.
    checksum_sha256   bytea CHECK (checksum_sha256 IS NULL OR octet_length(checksum_sha256) = 32),
    -- MD5 of the same bytes, in the form Active Storage keeps as `checksum`:
    -- declared optionally, checked against the bytes when it was, and
    -- computed on the way in otherwise. See DESIGN.md, "Creation".
    checksum_md5      bytea CHECK (checksum_md5 IS NULL OR octet_length(checksum_md5) = 16),
    size              bigint CHECK (size IS NULL OR size >= 0),
    skip_virus_scan   boolean NOT NULL DEFAULT false,
    -- Where a `url` blob's bytes are fetched from, kept for the retries and
    -- cleared once they are in. A presigned URL is a secret with an expiry:
    -- private metadata only, never in an error or a log. See DESIGN.md,
    -- "Fetched blobs".
    url               text,
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

    -- generated blobs: a `rows`, `archive` or `document` blob is built
    -- from a source that arrives in parts, stored under `<id>/source/<n>`
    -- while the blob is still `created`. See DESIGN.md, "Generated blobs".
    -- Parts accepted so far; the next one must be source_parts + 1.
    source_parts      integer NOT NULL DEFAULT 0 CHECK (source_parts >= 0),
    -- Lines across the parts: manifest entries for an archive or a document.
    source_rows       bigint  NOT NULL DEFAULT 0 CHECK (source_rows >= 0),
    -- Bytes of source across the parts.
    source_size       bigint  NOT NULL DEFAULT 0 CHECK (source_size >= 0),
    -- When the last part landed; the reaper expires a source left here.
    source_updated_at timestamptz,
    -- The sheets of a `rows` blob, each with its columns in order:
    -- [{"name": ..., "columns": [{"name": ..., "type": ...}, ...]}].
    -- Written at creation, read by accept and build. The source switches
    -- between sheets with `{"sheet": "..."}` lines, and the row limit is
    -- per sheet, so accept carries where it is across parts. See
    -- DESIGN.md, "Rows".
    source_columns    jsonb,
    -- Index, from zero, of the sheet the source is currently filling.
    source_sheet      integer NOT NULL DEFAULT 0 CHECK (source_sheet >= 0),
    -- Lines accepted into that sheet so far; `source_rows` stays the total.
    source_sheet_rows bigint  NOT NULL DEFAULT 0 CHECK (source_sheet_rows >= 0),
    -- Set by the archive build: members written, and members skipped
    -- because they were gone by then (listed in the archive's ERRORS.txt).
    archive_entries   bigint CHECK (archive_entries IS NULL OR archive_entries >= 0),
    archive_errors    bigint CHECK (archive_errors IS NULL OR archive_errors >= 0),
    -- The entry file of a `document` blob's workspace, relative to its
    -- root, declared at creation (`main.typ` by default). See DESIGN.md,
    -- "Documents".
    source_main       text,
    -- Bytes the workspace of a `document` blob takes once written out, the
    -- members' sizes plus the inline files, accumulated part by part and
    -- bounded by document.max_workspace_size.
    workspace_size    bigint  NOT NULL DEFAULT 0 CHECK (workspace_size >= 0),

    -- encryption (envelope: data key wrapped by the KEK named kek_id). The
    -- streaming AEAD header is cached on the row so a range request needs
    -- a single bucket read.
    kek_id            text,
    wrapped_key       bytea,
    encryption_header bytea,
    chunk_size        integer,

    -- derived representations, each with the variant version it was built
    -- with, so a bump can regenerate one without touching the other. A
    -- representation turned off at creation is `disabled` for good: never
    -- pending, so the render job and the lazy repair path leave it alone.
    thumbnail_state   text NOT NULL DEFAULT 'pending'
                      CHECK (thumbnail_state IN ('pending', 'ready', 'failed', 'unavailable', 'disabled')),
    preview_state     text NOT NULL DEFAULT 'pending'
                      CHECK (preview_state IN ('pending', 'ready', 'failed', 'unavailable', 'disabled')),
    preview_source    text
                      CHECK (preview_source IN ('image', 'embedded_image', 'geojson', 'gpx', 'kml')),
    thumbnail_variant_version integer,
    preview_variant_version   integer,
    -- What was rendered: the SHA-256 and the size of the WebP, recorded
    -- with `ready` and cleared with any other state, so a conditional or
    -- HEAD request is answered from the row without a bucket read.
    thumbnail_sha256  bytea CHECK (thumbnail_sha256 IS NULL OR octet_length(thumbnail_sha256) = 32),
    thumbnail_size    integer CHECK (thumbnail_size IS NULL OR thumbnail_size >= 0),
    preview_sha256    bytea CHECK (preview_sha256 IS NULL OR octet_length(preview_sha256) = 32),
    preview_size      integer CHECK (preview_size IS NULL OR preview_size >= 0),

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
    purged_at           timestamptz,

    -- The job that was to finish the blob (a scan, a build or a render) ran
    -- out of attempts: when, and its last error. Set when such a job dies,
    -- cleared when one of its kind is enqueued or retried for the blob, so
    -- a stuck blob stays visible after the dead job itself is pruned. See
    -- DESIGN.md, "Jobs".
    failed_at           timestamptz,
    last_error          text
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
-- Blobs whose job died, for `GET /blobs?failed=true`.
CREATE INDEX blobs_failed_idx       ON blobs (id)
    WHERE failed_at IS NOT NULL;
-- Reaper: expiring blobs.
CREATE INDEX blobs_expires_at_idx   ON blobs (expires_at)
    WHERE expires_at IS NOT NULL AND state NOT IN ('expired', 'deleted');
-- Reaper: stale uploads.
CREATE INDEX blobs_uploading_idx    ON blobs (upload_heartbeat_at)
    WHERE state = 'uploading';
-- Reaper: abandoned sources, and attached blobs whose file never came.
CREATE INDEX blobs_stale_source_idx ON blobs (source_updated_at)
    WHERE state = 'created' AND source_parts > 0;
CREATE INDEX blobs_abandoned_idx ON blobs (updated_at)
    WHERE state = 'created' AND source_parts = 0 AND expires_at IS NULL;
-- Reaper: terminal blobs whose objects are still in the bucket, the
-- original or the source parts a generated blob held.
CREATE INDEX blobs_unpurged_idx     ON blobs (id)
    WHERE (storage_key IS NOT NULL OR source_parts > 0) AND purged_at IS NULL
      AND state IN ('infected', 'expired', 'deleted');
-- Audit: the rows in the bucket's key order, `<shard>/<id>/<name>` with the
-- shard the low byte of the id (see `blob::key`), to merge with the listing.
CREATE INDEX blobs_shard_idx ON blobs ((get_byte(uuid_send(id), 15)), id);
-- Reaper: tombstones to prune, `blobs.keep_purged` after the purge, or
-- after the row went terminal when there was nothing to purge.
CREATE INDEX blobs_purged_at_idx ON blobs (purged_at)
    WHERE purged_at IS NOT NULL;
CREATE INDEX blobs_empty_tombstone_idx ON blobs (updated_at)
    WHERE state IN ('expired', 'deleted')
      AND storage_key IS NULL AND source_parts = 0 AND purged_at IS NULL;
