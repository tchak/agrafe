-- The signature clamd reported for an infected blob, so the backend learns
-- what was found from the metadata, whichever path scanned the file.
ALTER TABLE blobs ADD COLUMN scan_signature text;
