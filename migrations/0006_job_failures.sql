-- A job that used up its attempts stays for inspection, locked by the worker
-- that gave up and marked here, so it is never claimed again and never holds
-- the one pending slot per (kind, blob).
ALTER TABLE jobs ADD COLUMN failed_at timestamptz;
