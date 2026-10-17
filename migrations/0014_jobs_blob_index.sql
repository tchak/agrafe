-- The reaper asks, for every terminal blob still holding objects, whether
-- a purge job of any state exists for it; the pending-only unique index
-- does not answer that. The same index serves `GET /jobs?blob_id=`.
CREATE INDEX jobs_blob_idx ON jobs (blob_id, kind) WHERE blob_id IS NOT NULL;
