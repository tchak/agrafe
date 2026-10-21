//! KML and KMZ as content types: the types and how an XML head or a ZIP
//! head is told to be one. What the helper draws from them is the map
//! contract in [`crate::map`].

/// The content type of a KML file, Google Earth's XML.
pub const KML: &str = "application/vnd.google-earth.kml+xml";

/// The content type of a KMZ file: a KML document and its icons and
/// overlays in a ZIP.
pub const KMZ: &str = "application/vnd.google-earth.kmz";

/// Whether an XML document is a KML one: its root element, past the XML
/// declaration, comments and a doctype, is `<kml>` within the first KiB.
pub fn looks_like_kml(head: &[u8]) -> bool {
    crate::xml::root_element(head) == Some(b"kml")
}

/// Whether a ZIP is a KMZ one: its first local file header names a
/// `.kml` entry, `doc.kml` as Google Earth writes it. Entry order is the
/// writer's, so an archive that starts with an icon or a `__MACOSX/`
/// entry is not recognised, and a client that knows the field is a map
/// declares the type itself.
pub fn looks_like_kmz(head: &[u8]) -> bool {
    first_zip_entry(head)
        .is_some_and(|name| name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(b".kml"))
}

/// The name in the first local file header of a ZIP, when `head` starts
/// with one that fits in it.
fn first_zip_entry(head: &[u8]) -> Option<&[u8]> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_kml_head() {
        assert!(looks_like_kml(
            b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<kml xmlns=\"http://www.opengis.net/kml/2.2\">"
        ));
        assert!(looks_like_kml(b"\xef\xbb\xbf<kml>"));
        assert!(looks_like_kml(b"<kml/>"));
        assert!(looks_like_kml(
            b"<?xml version=\"1.0\"?>\n<!-- exported -->\n<kml:kml xmlns:kml=\"http://www.opengis.net/kml/2.2\">"
        ));
        assert!(!looks_like_kml(b"<?xml version=\"1.0\"?><gpx>"));
        assert!(!looks_like_kml(b"<kmlx>"));
        assert!(!looks_like_kml(b"<Document>"));
        assert!(!looks_like_kml(b"PK\x03\x04"));
        assert!(!looks_like_kml(b""));
    }

    /// A ZIP head whose first local file header names `name`.
    fn zip_head(name: &[u8]) -> Vec<u8> {
        let mut head = b"PK\x03\x04\x14\x00\x00\x00\x08\x00".to_vec();
        head.extend_from_slice(&[0; 16]);
        head.extend_from_slice(&(name.len() as u16).to_le_bytes());
        head.extend_from_slice(&0u16.to_le_bytes());
        head.extend_from_slice(name);
        head.extend_from_slice(b"\x03\x00data");
        head
    }

    #[test]
    fn recognises_a_kmz_head() {
        assert!(looks_like_kmz(&zip_head(b"doc.kml")));
        assert!(looks_like_kmz(&zip_head(b"Carte.KML")));
        assert!(looks_like_kmz(&zip_head(b"export/carte.kml")));
        assert!(!looks_like_kmz(&zip_head(b"files/icon.png")));
        // The AppleDouble sidecar a Mac zips first is named after the
        // document it shadows.
        assert!(looks_like_kmz(&zip_head(b"__MACOSX/._doc.kml")));
        assert!(!looks_like_kmz(&zip_head(b".kml")));
        assert!(!looks_like_kmz(&zip_head(b"doc.kmz")));
        assert!(!looks_like_kmz(&zip_head(b"word/document.xml")));
        assert!(!looks_like_kmz(&zip_head(b"")));
        // A header cut before its name, or not a header.
        assert!(!looks_like_kmz(&zip_head(b"doc.kml")[..32]));
        assert!(!looks_like_kmz(b"PK\x03\x04"));
        assert!(!looks_like_kmz(b"<kml/>"));
        assert!(!looks_like_kmz(b""));
    }
}
