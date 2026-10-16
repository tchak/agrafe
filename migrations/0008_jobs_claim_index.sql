-- The claim query filters on kind, due time and not-dead, and also takes
-- locked rows whose lease has expired, so the old partial index on
-- unlocked rows did not cover it. Dead jobs keep their lock and stay out
-- of this index.
DROP INDEX jobs_ready_idx;
CREATE INDEX jobs_claimable_idx ON jobs (kind, run_at) WHERE failed_at IS NULL;
