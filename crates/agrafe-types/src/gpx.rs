//! GPX as a content type: the type and how an XML head is told to be
//! one. What the helper draws from it is the map contract in
//! [`crate::map`].

/// The content type of a GPX file, the GPS exchange format.
pub const GPX: &str = "application/gpx+xml";

/// Bytes of an XML head looked at for the root element.
const HEAD_LEN: usize = 1024;

/// Whether an XML document is a GPX one: its root element, past the XML
/// declaration, comments and a doctype, is `<gpx>` within the first KiB.
pub fn looks_like_gpx(head: &[u8]) -> bool {
    let head = &head[..head.len().min(HEAD_LEN)];
    root_element(head) == Some(b"gpx")
}

/// The local name of the root element of `head`, when it starts one.
fn root_element(head: &[u8]) -> Option<&[u8]> {
    let mut rest = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
    loop {
        rest = rest.trim_ascii_start();
        rest = if let Some(after) = rest.strip_prefix(b"<?") {
            // The XML declaration, or a processing instruction.
            past(after, b"?>")?
        } else if let Some(after) = rest.strip_prefix(b"<!--") {
            past(after, b"-->")?
        } else if let Some(after) = rest.strip_prefix(b"<!") {
            // A doctype without an internal subset.
            past(after, b">")?
        } else {
            let name = rest.strip_prefix(b"<")?;
            let end = name
                .iter()
                .position(|b| b.is_ascii_whitespace() || matches!(b, b'>' | b'/'))?;
            let name = &name[..end];
            // The namespace prefix, if any, is not the name.
            let local = name.iter().rposition(|&b| b == b':').map_or(0, |i| i + 1);
            return Some(&name[local..]);
        };
    }
}

/// What follows the first `needle` in `haystack`.
fn past<'a>(haystack: &'a [u8], needle: &[u8]) -> Option<&'a [u8]> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|at| &haystack[at + needle.len()..])
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
