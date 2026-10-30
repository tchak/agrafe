//! Telling a ZIP-packaged format from the head of an archive: the name of
//! its first entry, which is what KMZ and zipped shapefiles are
//! recognised by.

/// The name in the first local file header of a ZIP, when `head` starts
/// with one that fits in it.
pub(crate) fn first_entry(head: &[u8]) -> Option<&[u8]> {
    const SIGNATURE: &[u8] = b"PK\x03\x04";
    const NAME_LEN: usize = 26;
    const NAME: usize = 30;
    if !head.starts_with(SIGNATURE) {
        return None;
    }
    let len = usize::from(u16::from_le_bytes([
        *head.get(NAME_LEN)?,
        *head.get(NAME_LEN + 1)?,
    ]));
    head.get(NAME..NAME + len)
}

/// Whether the first entry of the ZIP `head` has a name of more than its
/// extension that ends in one of `extensions`, `.kml` for instance, in
/// any case.
pub(crate) fn first_entry_ends_with(head: &[u8], extensions: &[&[u8]]) -> bool {
    first_entry(head).is_some_and(|name| {
        extensions.iter().any(|extension| {
            name.len() > extension.len()
                && name[name.len() - extension.len()..].eq_ignore_ascii_case(extension)
        })
    })
}
