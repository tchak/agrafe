//! Telling an XML format from the head of a document: the root element,
//! which is what GPX and KML are recognised by.

/// Bytes of an XML head looked at for the root element.
const HEAD_LEN: usize = 1024;

/// The local name of the root element of `head`, past the XML
/// declaration, comments and a doctype, when one starts within the first
/// KiB. A namespace prefix is not part of the name.
pub(crate) fn root_element(head: &[u8]) -> Option<&[u8]> {
    let head = &head[..head.len().min(HEAD_LEN)];
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
