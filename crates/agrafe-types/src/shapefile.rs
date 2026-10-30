//! The zipped shapefile as a content type: the type and how a ZIP head is
//! told to be one. What the helper draws from it is the map contract in
//! [`crate::map`].

/// The content type of a zipped shapefile: one or more `.shp` layers,
/// each with its `.shx`, `.dbf` and `.prj`, in a ZIP. Nothing is
/// registered; this is the spelling in use.
pub const SHAPEFILE: &str = "application/x-shapefile";

/// Whether a ZIP is a zipped shapefile: its first local file header names
/// a `.shp`, `.shx`, `.dbf`, `.prj` or `.cpg` entry, folder or not; the
/// `.cpg` because alphabetical order, which `zip -r` and most tools
/// produce, puts it first whenever there is one. Entry order is the
/// writer's, so an archive that starts with a readme, a folder entry or
/// a `__MACOSX/` entry is not recognised, and a client that knows the
/// field is a map declares the type itself.
pub fn looks_like_shapefile(head: &[u8]) -> bool {
    crate::zip::first_entry_ends_with(head, &[b".shp", b".shx", b".dbf", b".prj", b".cpg"])
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn recognises_a_zipped_shapefile_head() {
        for name in [
            &b"parcelles.shp"[..],
            b"parcelles.shx",
            b"parcelles.dbf",
            b"parcelles.prj",
            b"parcelles.cpg",
            b"export/Parcelles.SHP",
        ] {
            assert!(looks_like_shapefile(&zip_head(name)), "{name:?}");
        }
        assert!(!looks_like_shapefile(&zip_head(b".shp")));
        assert!(!looks_like_shapefile(&zip_head(b"parcelles.qmd")));
        assert!(!looks_like_shapefile(&zip_head(b"LISEZMOI.txt")));
        assert!(!looks_like_shapefile(&zip_head(b"export/")));
        assert!(!looks_like_shapefile(&zip_head(b"doc.kml")));
        assert!(!looks_like_shapefile(&zip_head(b"word/document.xml")));
        assert!(!looks_like_shapefile(&zip_head(b"parcelles.shp")[..32]));
        assert!(!looks_like_shapefile(b"PK\x03\x04"));
        assert!(!looks_like_shapefile(b""));
    }
}
