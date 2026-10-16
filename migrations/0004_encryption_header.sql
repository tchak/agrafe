-- The streaming AEAD header (salt and nonce prefix) is cached on the row so
-- a range request needs a single bucket read. It replaces the bare prefix.
ALTER TABLE blobs RENAME COLUMN nonce_prefix TO encryption_header;
