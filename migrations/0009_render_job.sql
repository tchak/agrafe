-- Thumbnail and preview are produced by one `render` job, the thumbnail
-- from the preview. Pending rows of the two old kinds become one render job
-- per blob; the unique index allows one unlocked row per (kind, blob).
DELETE FROM jobs preview USING jobs thumbnail
    WHERE preview.kind = 'preview' AND thumbnail.kind = 'thumbnail'
      AND preview.blob_id = thumbnail.blob_id
      AND preview.locked_at IS NULL AND thumbnail.locked_at IS NULL;
UPDATE jobs SET kind = 'render' WHERE kind IN ('thumbnail', 'preview');
