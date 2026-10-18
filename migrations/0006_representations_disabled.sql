-- A blob may be created with a representation turned off: `disabled` is
-- set at creation and never changes, so the render job and the lazy
-- repair path leave it alone. See DESIGN.md, "Creation".
ALTER TABLE blobs
    DROP CONSTRAINT blobs_thumbnail_state_check,
    ADD CONSTRAINT blobs_thumbnail_state_check
        CHECK (thumbnail_state IN ('pending', 'ready', 'failed', 'unavailable', 'disabled')),
    DROP CONSTRAINT blobs_preview_state_check,
    ADD CONSTRAINT blobs_preview_state_check
        CHECK (preview_state IN ('pending', 'ready', 'failed', 'unavailable', 'disabled'));
