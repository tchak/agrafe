-- Blobs are write-once. A blob is uploaded at most once, so there is nothing
-- to choose per blob and no object version to track: the original lives at
-- `<id>/original`.
ALTER TABLE blobs DROP COLUMN write_once;
ALTER TABLE blobs DROP COLUMN version;
