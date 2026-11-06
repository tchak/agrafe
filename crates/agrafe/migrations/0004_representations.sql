-- Derived representations: one row per blob and kind the blob asked for.
-- A kind left out of `representations` at creation has no row: that is
-- `disabled`, never stored. See DESIGN.md, "Derived representations".
CREATE TABLE representations (
    blob_id    uuid NOT NULL REFERENCES blobs (id) ON DELETE CASCADE,
    kind       text NOT NULL CHECK (kind IN ('thumbnail', 'preview')),
    state      text NOT NULL DEFAULT 'pending'
               CHECK (state IN ('pending', 'ready', 'failed', 'unavailable')),
    -- The kind's version it was last built with, `media.variant_version`
    -- for the pictures, so a bump can regenerate one without touching the
    -- others. NULL before the first render.
    version    integer,
    -- What was built: the format it was encoded in, `media.format` at the
    -- time, which names what is served whatever the setting says today,
    -- and the SHA-256 and the size of the picture, recorded with `ready`
    -- and cleared with any other state, so a conditional or HEAD request
    -- is answered from the row without a bucket read. The CHECKs below
    -- hold them to that.
    format     text CHECK (format IN ('webp', 'jxl')),
    sha256     bytea CHECK (sha256 IS NULL OR octet_length(sha256) = 32),
    size       bigint CHECK (size IS NULL OR size >= 0),
    -- Preview only: what it was rendered from, and whether a `failed` one
    -- ran a tool out of its time limit (true, or NULL for every other row,
    -- which costs them nothing). A thumbnail rendered later over that
    -- verdict must not hand the file to a second tool for as long again.
    -- See DESIGN.md, "Rendering".
    source     text CHECK (source IN ('image', 'embedded_image', 'geojson', 'gpx', 'kml', 'shapefile', 'gml')),
    timed_out  boolean CHECK (timed_out),
    -- By the trigger, for the audit: when this representation last moved.
    updated_at timestamptz NOT NULL DEFAULT now(),
    -- The one-per-kind guarantee and the lookup path: nothing refers to a
    -- representation by an id of its own.
    PRIMARY KEY (blob_id, kind),
    CHECK ((state = 'ready') = (format IS NOT NULL)),
    CHECK ((state = 'ready') = (sha256 IS NOT NULL)),
    CHECK ((state = 'ready') = (size IS NOT NULL))
);

CREATE TRIGGER representations_updated_at
    BEFORE UPDATE ON representations
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
