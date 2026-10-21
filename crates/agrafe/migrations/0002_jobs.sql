-- Jobs: the Postgres-backed queue. See DESIGN.md, "Jobs".
CREATE TABLE jobs (
    id           bigserial PRIMARY KEY,
    kind         text NOT NULL
                 CHECK (kind IN ('fetch', 'scan', 'render', 'rows', 'archive', 'document', 'purge', 'reap', 'notify', 'audit')),
    blob_id      uuid REFERENCES blobs (id) ON DELETE CASCADE,
    -- Orders a kind's own queue (1 high, 0 normal, -1 low); kinds stay the
    -- isolation between job types. Copied from the blob, 0 without one.
    priority     smallint NOT NULL DEFAULT 0
                 CHECK (priority IN (-1, 0, 1)),
    payload      jsonb NOT NULL DEFAULT '{}'::jsonb,
    run_at       timestamptz NOT NULL DEFAULT now(),
    attempts     integer NOT NULL DEFAULT 0,
    max_attempts integer NOT NULL DEFAULT 5,
    locked_at    timestamptz,
    locked_by    text,
    last_error   text,
    -- A job that used up its attempts stays for inspection, locked by the
    -- worker that gave up and marked here, so it is never claimed again and
    -- never holds the one pending slot per (kind, blob).
    failed_at    timestamptz,
    -- The W3C traceparent of the request or job that enqueued this one, so
    -- that its run joins that trace. NULL when nothing was being traced.
    traceparent  text,
    created_at   timestamptz NOT NULL DEFAULT now()
);

-- The claim: highest priority first, oldest within it, by kind, not dead.
-- Locked rows whose lease has lapsed are claimable too, so the index does
-- not exclude them.
CREATE INDEX jobs_claimable_idx ON jobs (kind, priority DESC, run_at) WHERE failed_at IS NULL;
-- One pending job of each kind per blob; enqueue uses ON CONFLICT DO
-- NOTHING, and a `notify` replaces the pending one with the newer event.
-- Blob-less jobs dedupe too, so the periodic `reap` never piles up however
-- many processes start. Needs PostgreSQL 15.
CREATE UNIQUE INDEX jobs_pending_per_blob_idx ON jobs (kind, blob_id)
    NULLS NOT DISTINCT WHERE locked_at IS NULL;
-- The reaper asks whether a purge job of any state exists for a blob, and
-- `GET /jobs?blob_id=` lists by blob.
CREATE INDEX jobs_blob_idx ON jobs (blob_id, kind) WHERE blob_id IS NOT NULL;
