-- Each derived representation records the variant version it was built with,
-- so a bump can regenerate one without touching the other.
ALTER TABLE blobs RENAME COLUMN variant_version TO thumbnail_variant_version;
ALTER TABLE blobs ADD COLUMN preview_variant_version integer;
