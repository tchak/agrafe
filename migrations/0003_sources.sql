-- Generated blobs: a `rows` or `archive` blob is built from a source that
-- arrives in parts, stored under `<id>/source/<n>` while the blob is still
-- `created`. See DESIGN.md, "Generated blobs".
ALTER TABLE blobs
    -- Parts accepted so far; the next one must be source_parts + 1.
    ADD COLUMN source_parts      integer NOT NULL DEFAULT 0 CHECK (source_parts >= 0),
    -- Lines across the parts: manifest entries for an archive.
    ADD COLUMN source_rows       bigint  NOT NULL DEFAULT 0 CHECK (source_rows >= 0),
    -- Bytes of source across the parts.
    ADD COLUMN source_size       bigint  NOT NULL DEFAULT 0 CHECK (source_size >= 0),
    -- When the last part landed; the reaper expires a source left here.
    ADD COLUMN source_updated_at timestamptz,
    -- Set by the archive build: members written, and members skipped
    -- because they were gone by then (listed in the archive's ERRORS.txt).
    ADD COLUMN archive_entries   bigint CHECK (archive_entries IS NULL OR archive_entries >= 0),
    ADD COLUMN archive_errors    bigint CHECK (archive_errors IS NULL OR archive_errors >= 0);

-- Reaper: abandoned sources.
CREATE INDEX blobs_stale_source_idx ON blobs (source_updated_at)
    WHERE state = 'created' AND source_parts > 0;

-- A `created` blob holding parts has objects to purge too, so "holds
-- objects" is no longer `storage_key IS NOT NULL` alone.
DROP INDEX blobs_unpurged_idx;
CREATE INDEX blobs_unpurged_idx ON blobs (id)
    WHERE (storage_key IS NOT NULL OR source_parts > 0) AND purged_at IS NULL
      AND state IN ('infected', 'expired', 'deleted');
