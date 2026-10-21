//! GeoJSON as a content type: the type and how a JSON head is told to be
//! one. What the helper draws from it is the map contract in
//! [`crate::map`].

/// The content type of a GeoJSON file, RFC 7946.
pub const GEOJSON: &str = "application/geo+json";

/// Bytes of a JSON head looked at for the GeoJSON markers.
const HEAD_LEN: usize = 1024;

/// Whether a JSON document is a GeoJSON one: an object whose first KiB
/// carries `"type": "FeatureCollection"` or `"type": "Feature"`. A bare
/// geometry at the top level is not recognised here, only when declared.
pub fn looks_like_geojson(head: &[u8]) -> bool {
    let head = &head[..head.len().min(HEAD_LEN)];
    let body = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
    let mut i = 0;
    while i < body.len() && body[i].is_ascii_whitespace() {
        i += 1;
    }
    if body.get(i) != Some(&b'{') {
        return false;
    }
    let mut rest = &body[i..];
    while let Some(at) = find(rest, b"\"type\"") {
        rest = &rest[at + b"\"type\"".len()..];
        let value = rest.trim_ascii_start();
        let Some(value) = value.strip_prefix(b":") else {
            continue;
        };
        let value = value.trim_ascii_start();
        if value.starts_with(b"\"FeatureCollection\"") || value.starts_with(b"\"Feature\"") {
            return true;
        }
    }
    false
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_geojson_head() {
        assert!(looks_like_geojson(
            br#"{"type": "FeatureCollection", "features": []}"#
        ));
        assert!(looks_like_geojson(
            b"\xef\xbb\xbf \n{\n  \"type\" :\n \"Feature\""
        ));
        // The marker may come after other keys.
        assert!(looks_like_geojson(
            br#"{"name": "plots", "crs": {"type": "name"}, "type": "FeatureCollection"}"#
        ));
        assert!(!looks_like_geojson(
            br#"{"type": "Point", "coordinates": [1, 2]}"#
        ));
        assert!(!looks_like_geojson(br#"{"kind": "Feature"}"#));
        assert!(!looks_like_geojson(br#"["Feature"]"#));
        assert!(!looks_like_geojson(b"type: Feature"));
        assert!(!looks_like_geojson(b""));
        // Past the first KiB is not looked at.
        let mut late = b"{\"a\": \"".to_vec();
        late.extend(std::iter::repeat_n(b'x', 1100));
        late.extend_from_slice(b"\", \"type\": \"Feature\"}");
        assert!(!looks_like_geojson(&late));
    }
}
