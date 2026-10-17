-- Priority orders a kind's own queue; kinds stay the isolation between job
-- types. A blob's priority is copied onto the jobs enqueued for it.
ALTER TABLE blobs ADD COLUMN priority text NOT NULL DEFAULT 'normal'
    CHECK (priority IN ('high', 'normal', 'low'));
ALTER TABLE jobs ADD COLUMN priority smallint NOT NULL DEFAULT 0;

-- The claim takes the highest priority first, oldest within it.
DROP INDEX jobs_claimable_idx;
CREATE INDEX jobs_claimable_idx ON jobs (kind, priority DESC, run_at) WHERE failed_at IS NULL;
