-- Keep blobs.updated_at current on every UPDATE so no code path has to.
CREATE FUNCTION set_updated_at() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END
$$;

CREATE TRIGGER blobs_updated_at
    BEFORE UPDATE ON blobs
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
