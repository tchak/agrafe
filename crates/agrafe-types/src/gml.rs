//! GML as a content type: the type and how an XML head is told to be
//! one. What the helper draws from it is the map contract in
//! [`crate::map`].

/// The content type of a GML file, the OGC's geography markup, what a WFS
/// serves.
pub const GML: &str = "application/gml+xml";

/// The namespaces of GML: 2 and 3.1 share one, 3.2 has its own.
pub const NAMESPACES: [&str; 2] = [
    "http://www.opengis.net/gml",
    "http://www.opengis.net/gml/3.2",
];

/// Whether an XML document is a GML one. GML has no root element of its
/// own: a document is a `wfs:FeatureCollection`, a `gml:FeatureCollection`
/// or an element of an application schema, so what is looked for is the
/// root element declaring a GML namespace, within the first KiB. A root
/// whose declarations run past it is not recognised, and the client
/// declares the type.
pub fn looks_like_gml(head: &[u8]) -> bool {
    crate::xml::root_declares(head, &NAMESPACES.map(str::as_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_gml_head() {
        // A WFS 2.0 response.
        assert!(looks_like_gml(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<wfs:FeatureCollection xmlns:wfs="http://www.opengis.net/wfs/2.0"
    xmlns:gml="http://www.opengis.net/gml/3.2"
    xmlns:cadastre="https://example.org/cadastre" numberMatched="2">"#
        ));
        // GML 2, as an OGR export.
        assert!(looks_like_gml(
            b"<?xml version=\"1.0\"?>\n<ogr:FeatureCollection xmlns:ogr=\"http://ogr.maptools.org/\" xmlns:gml='http://www.opengis.net/gml'>"
        ));
        // The default namespace, an empty root, a comment first.
        assert!(looks_like_gml(
            b"\xef\xbb\xbf<!-- x --><FeatureCollection xmlns=\"http://www.opengis.net/gml\"/>"
        ));
        assert!(looks_like_gml(
            b"<cadastre:Parcelle\n\txmlns:cadastre = \"x\"\txmlns:gml=\"http://www.opengis.net/gml/3.2\" gml:id=\"p1\">"
        ));
        // Another namespace of the same family is not GML.
        assert!(!looks_like_gml(
            b"<c xmlns:gmlcov=\"http://www.opengis.net/gmlcov/1.0\">"
        ));
        assert!(!looks_like_gml(
            b"<c xmlns:gml=\"http://www.opengis.net/gml/3.3\">"
        ));
        // Not a declaration, not on the root, not a namespace attribute.
        assert!(!looks_like_gml(
            b"<c><d xmlns:gml=\"http://www.opengis.net/gml\"/></c>"
        ));
        assert!(!looks_like_gml(
            b"<c schemaLocation=\"http://www.opengis.net/gml\">"
        ));
        assert!(!looks_like_gml(
            b"<c xmlnsgml=\"http://www.opengis.net/gml\">"
        ));
        assert!(!looks_like_gml(
            b"<kml xmlns=\"http://www.opengis.net/kml/2.2\">"
        ));
        // A start tag that does not end within the head, or is not one.
        assert!(!looks_like_gml(
            b"<c xmlns:gml=\"http://www.opengis.net/gml\""
        ));
        assert!(!looks_like_gml(
            b"<c xmlns:gml=\"http://www.opengis.net/gml>"
        ));
        assert!(!looks_like_gml(b"<c xmlns:gml=http://www.opengis.net/gml>"));
        let mut late = b"<c a=\"".to_vec();
        late.extend(std::iter::repeat_n(b'x', 1100));
        late.extend_from_slice(b"\" xmlns:gml=\"http://www.opengis.net/gml\">");
        assert!(!looks_like_gml(&late));
        assert!(!looks_like_gml(b"http://www.opengis.net/gml"));
        assert!(!looks_like_gml(b""));
    }
}
