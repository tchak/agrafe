-- An upload in flight touches its row while it runs, so the reaper can tell
-- a slow upload from one a crashed process left behind.
ALTER TABLE blobs ADD COLUMN upload_heartbeat_at timestamptz;
DROP INDEX blobs_uploading_idx;
CREATE INDEX blobs_uploading_idx ON blobs (upload_heartbeat_at) WHERE state = 'uploading';
