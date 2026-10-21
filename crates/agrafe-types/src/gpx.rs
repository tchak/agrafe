//! GPX as a content type: the type and how an XML head is told to be
//! one. What the helper draws from it is the map contract in
//! [`crate::map`].

/// The content type of a GPX file, the GPS exchange format.
pub const GPX: &str = "application/gpx+xml";

/// Whether an XML document is a GPX one: its root element, past the XML
/// declaration, comments and a doctype, is `<gpx>` within the first KiB.
pub fn looks_like_gpx(head: &[u8]) -> bool {
    crate::xml::root_element(head) == Some(b"gpx")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_gpx_head() {
        assert!(looks_like_gpx(
            b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<gpx version=\"1.1\" creator=\"x\">"
        ));
        assert!(looks_like_gpx(b"\xef\xbb\xbf<gpx>"));
        assert!(looks_like_gpx(b"<gpx/>"));
        assert!(looks_like_gpx(
            b"<?xml version=\"1.0\"?>\n<!-- exported -->\n<!DOCTYPE gpx>\n<gpx\n xmlns=\"http://www.topografix.com/GPX/1/1\">"
        ));
        assert!(looks_like_gpx(
            b"<g:gpx xmlns:g=\"http://www.topografix.com/GPX/1/1\">"
        ));
        assert!(!looks_like_gpx(b"<?xml version=\"1.0\"?><kml>"));
        assert!(!looks_like_gpx(b"<gpxx>"));
        assert!(!looks_like_gpx(
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"
        ));
        assert!(!looks_like_gpx(br#"{"type": "Feature"}"#));
        assert!(!looks_like_gpx(b"<?xml version=\"1.0\"?>"));
        assert!(!looks_like_gpx(b"<!-- unterminated"));
        assert!(!looks_like_gpx(b"gpx"));
        assert!(!looks_like_gpx(b""));
        // Past the first KiB is not looked at.
        let mut late = b"<!-- ".to_vec();
        late.extend(std::iter::repeat_n(b'x', 1100));
        late.extend_from_slice(b" --><gpx>");
        assert!(!looks_like_gpx(&late));
    }
}
