//! The content types an installation accepts files of, `upload.content_types`.

use std::sync::Arc;

use serde::Deserialize;

use crate::sniff;

const OCTET_STREAM: &str = "application/octet-stream";

/// A list of content types, exact or `type/*`, that the files a blob is
/// created for must have. Empty, the default, accepts any. An exact entry
/// also accepts its aliases (`image/jpg` for `image/jpeg`), and nothing
/// else: listing XLSX does not admit the macro-enabled XLSM.
///
/// Cheap to clone: an upload holds one for its checks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "Vec<String>")]
pub struct Allowlist(Arc<Vec<Entry>>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// A canonical `type/subtype`.
    Exact(String),
    /// The `type` of a `type/*`.
    Type(String),
}

impl Allowlist {
    /// Whether the list is empty, accepting any content type.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether a blob may be declared as `content_type`, a normalised
    /// `type/subtype`.
    pub fn allows(&self, content_type: &str) -> bool {
        let wanted = sniff::canonical(content_type);
        self.is_empty()
            || self.0.iter().any(|entry| match entry {
                Entry::Exact(e) => e == wanted,
                Entry::Type(t) => major(wanted) == t,
            })
    }

    /// Whether bytes sniffed as `sniffed` may be stored for a blob declared
    /// as `declared`, once the two were found compatible. Only a
    /// declaration of `application/octet-stream` is compatible with any
    /// bytes, so only that one is checked further: the bytes must be a
    /// listed type, or packaged in one (listing ZIP admits a DOCX). Not the
    /// other way round, and none of the leniency `sniff::compatible` has
    /// for a declaration: listing DOCX does not admit a bare ZIP, listing
    /// `text/csv` does not admit HTML. The declaration said nothing, so
    /// there is nothing to be lenient towards. Listing
    /// `application/octet-stream` admits the files nothing recognises, not
    /// every file.
    pub fn allows_sniffed(&self, declared: &str, sniffed: &str) -> bool {
        if self.is_empty() || declared != OCTET_STREAM {
            return true;
        }
        let s = sniff::canonical(sniffed);
        self.0.iter().any(|entry| match entry {
            // The sniffer reports a variant as its base format, so an
            // entry for the variant admits that format's bytes.
            Entry::Exact(e) => {
                sniff::base(e).unwrap_or(e) == s || sniff::container(s) == Some(e.as_str())
            }
            Entry::Type(t) => major(s) == t,
        })
    }
}

impl TryFrom<Vec<String>> for Allowlist {
    type Error = String;

    fn try_from(raw: Vec<String>) -> Result<Self, String> {
        raw.iter()
            .map(|s| parse(s))
            .collect::<Result<Vec<_>, _>>()
            .map(|entries| Self(Arc::new(entries)))
    }
}

fn parse(raw: &str) -> Result<Entry, String> {
    let lower = raw.trim().to_ascii_lowercase();
    let invalid = || format!("`{raw}` is not a content type (`type/subtype` or `type/*`)");
    let (major, minor) = lower.split_once('/').ok_or_else(invalid)?;
    if !token(major) {
        return Err(if major == "*" {
            format!("`{raw}`: leave the list empty to accept any content type")
        } else {
            invalid()
        });
    }
    if minor == "*" {
        return Ok(Entry::Type(major.to_string()));
    }
    if !token(minor) {
        return Err(invalid());
    }
    Ok(Entry::Exact(sniff::canonical(&lower).to_string()))
}

/// An RFC 6838 name: what a `type` or a `subtype` may be made of.
fn token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
}

fn major(content_type: &str) -> &str {
    content_type
        .split_once('/')
        .map_or(content_type, |(t, _)| t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(entries: &[&str]) -> Allowlist {
        Allowlist::try_from(entries.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn empty_accepts_anything() {
        let any = Allowlist::default();
        assert!(any.allows("application/x-msdownload"));
        assert!(any.allows_sniffed(OCTET_STREAM, "application/x-executable"));
    }

    #[test]
    fn exact_entries_and_their_aliases() {
        let list = list(&["image/jpeg", " Application/PDF ", "text/rtf"]);
        assert!(list.allows("image/jpeg"));
        assert!(list.allows("image/jpg"));
        assert!(list.allows("application/pdf"));
        assert!(list.allows("application/x-pdf"));
        // The alias is listed; the canonical type is what is compared.
        assert!(list.allows("application/rtf"));
        assert!(!list.allows("image/png"));
        assert!(!list.allows(OCTET_STREAM));
    }

    #[test]
    fn variants_are_not_their_base() {
        let xlsx = list(&["application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"]);
        assert!(!xlsx.allows("application/vnd.ms-excel.sheet.macroenabled.12"));
        // What the sniffer reports a format as, or packages it in, is not
        // a spelling of it, in either direction.
        for (one, other) in [
            ("image/heif", "image/heic"),
            ("font/woff", "font/woff2"),
            ("font/ttf", "font/otf"),
            ("font/sfnt", "font/ttf"),
            ("video/webm", "video/x-matroska"),
            ("application/zip", "application/vnd.android.package-archive"),
        ] {
            assert!(!list(&[one]).allows(other), "{one} admits {other}");
            assert!(!list(&[other]).allows(one), "{other} admits {one}");
        }
        // A spelling is.
        assert!(list(&["application/font-woff"]).allows("font/woff"));
        assert!(list(&["font/ttf"]).allows("application/x-font-ttf"));
    }

    #[test]
    fn wildcards_cover_a_type() {
        let list = list(&["image/*", "text/csv"]);
        assert!(list.allows("image/heic"));
        assert!(list.allows("image/svg+xml"));
        assert!(!list.allows("text/plain"));
        assert!(!list.allows("application/pdf"));
    }

    #[test]
    fn octet_stream_admits_only_what_a_listed_type_would() {
        let list = list(&[
            OCTET_STREAM,
            "application/pdf",
            "image/*",
            "application/zip",
        ]);
        assert!(list.allows(OCTET_STREAM));
        assert!(list.allows_sniffed(OCTET_STREAM, "application/pdf"));
        assert!(list.allows_sniffed(OCTET_STREAM, "image/png"));
        // A ZIP entry covers what is packaged in one.
        assert!(list.allows_sniffed(
            OCTET_STREAM,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        ));
        assert!(!list.allows_sniffed(OCTET_STREAM, "application/x-executable"));
        assert!(!list.allows_sniffed(OCTET_STREAM, "text/html"));
        // Any other declaration was already checked against the bytes.
        assert!(list.allows_sniffed("application/pdf", "application/pdf"));
    }

    /// The leniency `sniff::compatible` has for a declaration is for a
    /// declaration; `application/octet-stream` made none.
    #[test]
    fn octet_stream_gets_none_of_the_declared_leniency() {
        const DOCX: &str =
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
        const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
        const GEOJSON: &str = "application/geo+json";
        // A DOCX is a ZIP; a ZIP is not a DOCX.
        let docx = list(&[DOCX]);
        assert!(docx.allows_sniffed(OCTET_STREAM, DOCX));
        assert!(!docx.allows_sniffed(OCTET_STREAM, "application/zip"));
        assert!(!docx.allows_sniffed(OCTET_STREAM, XLSX));
        // Text sniffers are heuristics against a text declaration, not
        // against no declaration.
        let csv = list(&["text/csv"]);
        assert!(!csv.allows_sniffed(OCTET_STREAM, "text/html"));
        assert!(!csv.allows_sniffed(OCTET_STREAM, "text/x-shellscript"));
        assert!(!csv.allows_sniffed(OCTET_STREAM, GEOJSON));
        for text in [
            list(&["text/plain"]),
            list(&["text/*"]),
            list(&["application/json"]),
        ] {
            assert!(!text.allows_sniffed(OCTET_STREAM, GEOJSON));
        }
        assert!(list(&[GEOJSON]).allows_sniffed(OCTET_STREAM, GEOJSON));
        assert!(list(&["application/*"]).allows_sniffed(OCTET_STREAM, GEOJSON));
        // A variant sniffs as its base, so listing it admits those bytes.
        let xlsm = list(&["application/vnd.ms-excel.sheet.macroenabled.12"]);
        assert!(xlsm.allows_sniffed(OCTET_STREAM, XLSX));
        assert!(!xlsm.allows_sniffed(OCTET_STREAM, DOCX));
        assert!(list(&["image/heic"]).allows_sniffed(OCTET_STREAM, "image/heif"));
        assert!(list(&["font/woff2"]).allows_sniffed(OCTET_STREAM, "application/font-woff"));
        assert!(!list(&["font/woff2"]).allows_sniffed(OCTET_STREAM, "application/font-sfnt"));
        // Every OLE file sniffs as the storage, an MSI installer like a Word
        // 97 document, so a Word 97 entry admits none of them undeclared;
        // the storage itself can be listed.
        const OLE: &str = "application/x-ole-storage";
        assert!(!list(&["application/msword"]).allows_sniffed(OCTET_STREAM, OLE));
        assert!(list(&[OLE]).allows_sniffed(OCTET_STREAM, OLE));
        // Aliases on either side.
        assert!(list(&["image/jpg"]).allows_sniffed(OCTET_STREAM, "image/jpeg"));
        assert!(list(&["application/pdf"]).allows_sniffed(OCTET_STREAM, "application/x-pdf"));
    }

    #[test]
    fn rejects_what_is_not_a_content_type() {
        for bad in [
            "pdf",
            "*/*",
            "*/pdf",
            "image/",
            "/png",
            "text/plain; charset=utf-8",
            "",
        ] {
            assert!(Allowlist::try_from(vec![bad.to_string()]).is_err(), "{bad}");
        }
        let err = Allowlist::try_from(vec!["*/*".to_string()]).unwrap_err();
        assert!(err.contains("leave the list empty"), "{err}");
    }

    #[test]
    fn deserializes_from_a_list() {
        let list: Allowlist = serde_json::from_str(r#"["image/*", "application/pdf"]"#).unwrap();
        assert!(list.allows("image/png"));
        assert!(serde_json::from_str::<Allowlist>(r#"["nope"]"#).is_err());
    }
}
