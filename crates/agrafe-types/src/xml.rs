//! Telling an XML format from the head of a document: the root element,
//! which is what GPX and KML are recognised by, and the namespaces it
//! declares, which is what GML is recognised by.

/// Bytes of an XML head looked at for the root element.
const HEAD_LEN: usize = 1024;

/// The local name of the root element of `head`, past the XML
/// declaration, comments and a doctype, when one starts within the first
/// KiB. A namespace prefix is not part of the name.
pub(crate) fn root_element(head: &[u8]) -> Option<&[u8]> {
    let tag = root_tag(head)?;
    let end = tag
        .iter()
        .position(|b| b.is_ascii_whitespace() || matches!(b, b'>' | b'/'))?;
    let name = &tag[..end];
    // The namespace prefix, if any, is not the name.
    let local = name.iter().rposition(|&b| b == b':').map_or(0, |i| i + 1);
    Some(&name[local..])
}

/// Whether the root element of `head` declares one of `namespaces`, as
/// its default namespace or under a prefix. Its start tag must end
/// within the first KiB: a root whose declarations run past it is not
/// looked into.
pub(crate) fn root_declares(head: &[u8], namespaces: &[&[u8]]) -> bool {
    root_tag(head)
        .and_then(attributes)
        .is_some_and(|attributes| {
            attributes.iter().any(|(name, value)| {
                (*name == b"xmlns" || name.starts_with(b"xmlns:")) && namespaces.contains(value)
            })
        })
}

/// The root element's start tag in `head`, from its name to the end of
/// the head, past the XML declaration, comments and a doctype.
fn root_tag(head: &[u8]) -> Option<&[u8]> {
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
            return rest.strip_prefix(b"<");
        };
    }
}

/// The attributes of the start tag `tag`, from its name on, as names and
/// raw values. `None` when the tag does not end in `tag` or is not one.
fn attributes(tag: &[u8]) -> Option<Vec<(&[u8], &[u8])>> {
    let name_end = tag
        .iter()
        .position(|b| b.is_ascii_whitespace() || matches!(b, b'>' | b'/'))?;
    let mut rest = &tag[name_end..];
    let mut attributes = Vec::new();
    loop {
        rest = rest.trim_ascii_start();
        if rest.starts_with(b">") || rest.starts_with(b"/>") {
            return Some(attributes);
        }
        let eq = rest.iter().position(|&b| b == b'=')?;
        let name = rest[..eq].trim_ascii_end();
        if name.is_empty() || name.iter().any(|b| b.is_ascii_whitespace()) {
            return None;
        }
        let value = rest[eq + 1..].trim_ascii_start();
        let quote = *value.first().filter(|q| matches!(q, b'"' | b'\''))?;
        let len = value[1..].iter().position(|&b| b == quote)?;
        attributes.push((name, &value[1..1 + len]));
        rest = &value[len + 2..];
    }
}

/// What follows the first `needle` in `haystack`.
fn past<'a>(haystack: &'a [u8], needle: &[u8]) -> Option<&'a [u8]> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|at| &haystack[at + needle.len()..])
}
