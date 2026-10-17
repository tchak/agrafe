-- One representation for priority on both tables: the rank the claim
-- orders on (1 high, 0 normal, -1 low), rather than text on blobs and a
-- rank on jobs.
ALTER TABLE blobs DROP CONSTRAINT blobs_priority_check;
ALTER TABLE blobs ALTER COLUMN priority DROP DEFAULT;
ALTER TABLE blobs ALTER COLUMN priority TYPE smallint
    USING CASE priority WHEN 'high' THEN 1 WHEN 'low' THEN -1 ELSE 0 END;
ALTER TABLE blobs ALTER COLUMN priority SET DEFAULT 0;
ALTER TABLE blobs ADD CONSTRAINT blobs_priority_check CHECK (priority IN (-1, 0, 1));
ALTER TABLE jobs ADD CONSTRAINT jobs_priority_check CHECK (priority IN (-1, 0, 1));
