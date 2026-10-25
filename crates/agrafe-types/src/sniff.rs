//! Magic-byte sniffing of uploads, compared against the declared content type.
//!
//! The check is deliberately lenient where the declared type is a container
//! of the sniffed one (a DOCX is a ZIP), where the two are aliases
//! (`image/jpg`), and for text, which has no magic. It only rejects what is
//! clearly another kind of file.
//!
//! `loader_for` answers a different question for the renderer: whether the
//! bytes are something vips may be handed at all, and with which loader.

use crate::geojson::{self, GEOJSON};
use crate::gpx::GPX;
use crate::kml::{self, KML, KMZ};

/// Bytes needed before a verdict. PDF allows its magic anywhere in the first
/// 1 KiB; ZIP-based office formats need a few local file headers.
pub const SNIFF_LEN: usize = 8 * 1024;

/// The content type of an OLE compound file, the container of the Office
/// 97-2003 formats, Outlook messages, Visio and Publisher files.
const OLE: &str = "application/x-ole-storage";

/// The content type of a ZIP archive, the container of the Office Open
/// XML and OpenDocument packages among others.
const ZIP: &str = "application/zip";

/// The content type of an XML document, the container of SVG, GPX, KML and
/// every `+xml` type.
const XML: &str = "text/xml";

/// What the first bytes of an upload look like, if recognisable. JSON has
/// no magic, but a JSON document that opens on a Feature or a
/// FeatureCollection is a GeoJSON one, and is sniffed as such so that a
/// `.geojson` declared as JSON or text can be refined (see [`refine`]).
/// XML has a text sniffer that fires on the declaration; an XML document
/// whose root element is `<gpx>` or `<kml>` is a GPX or a KML one, for
/// the same reason. A ZIP whose first entry is a `.kml` is a KMZ.
///
/// An OLE compound file sniffs as the generic storage: without its `std`
/// feature, which would parse the file's directory in-process, `infer`
/// calls every one of them a Word document, an Excel workbook, an Outlook
/// message and a Visio drawing alike. The Excel and PowerPoint types are
/// remapped too, so that the feature flipping on through unification
/// would cost the in-process parse and nothing else.
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    let inferred = infer::get(head).map(|kind| kind.mime_type());
    match inferred {
        Some(
            "application/msword" | "application/vnd.ms-excel" | "application/vnd.ms-powerpoint",
        ) => {
            return Some(OLE);
        }
        Some("application/zip") if kml::looks_like_kmz(head) => return Some(KMZ),
        Some("text/xml") | None => {}
        Some(other) => return Some(other),
    }
    if geojson::looks_like_geojson(head) {
        return Some(GEOJSON);
    }
    // One look at the XML head for both root elements.
    match crate::xml::root_element(head) {
        Some(b"gpx") => Some(GPX),
        Some(b"kml") => Some(KML),
        _ => inferred,
    }
}

/// The content type a blob declared as `declared` gets once its bytes
/// sniffed as `sniffed`: the sniffed type when it is more precise than a
/// declaration that covers it, as a GeoJSON file declared as JSON or plain
/// text, a GPX or KML file declared as XML or plain text, or a KMZ file
/// declared as a ZIP; `None` to keep the declaration. The category is the
/// same either way.
pub fn refine(declared: &str, sniffed: &str) -> Option<&'static str> {
    match (canonical(declared), sniffed) {
        ("application/json" | "text/json" | "text/plain", GEOJSON) => Some(GEOJSON),
        ("text/xml" | "text/plain", GPX) => Some(GPX),
        ("text/xml" | "text/plain", KML) => Some(KML),
        ("application/zip", KMZ) => Some(KMZ),
        _ => None,
    }
}

/// The content type that declares nothing, compatible with any bytes.
pub const OCTET_STREAM: &str = "application/octet-stream";

/// Whether bytes sniffed as `sniffed` may be served as `declared`.
pub fn compatible(declared: &str, sniffed: &str) -> bool {
    let d = canonical(declared);
    if d == OCTET_STREAM {
        return true;
    }
    let d = base(d).unwrap_or(d);
    let s = canonical(sniffed);
    if d == s {
        return true;
    }
    if container(d) == Some(s) || container(s) == Some(d) {
        return true;
    }
    // A GeoJSON file is JSON, and text.
    if s == GEOJSON {
        return matches!(d, "application/json" | "text/json") || d.starts_with("text/");
    }
    // A GPX or KML file is XML, which the container rule covers, and text.
    if s == GPX || s == KML {
        return d.starts_with("text/");
    }
    // The text sniffers (HTML, XML, shell scripts) are heuristics on the
    // first bytes: an SVG that opens on a comment sniffs as HTML, a script
    // declared as JavaScript as a shell script. They contradict a
    // declaration only when that one has magic of its own, since its real
    // bytes would have sniffed as it.
    s.starts_with("text/") && !has_magic(d)
}

/// Whether a file of this type opens on bytes the sniffer recognises: a
/// type `infer` has a matcher for, or one packaged in a ZIP or an OLE
/// compound file. The XML-packaged types open on text, like JSON, scripts
/// and everything the sniffer does not know; so do the text sniffers'
/// own types, which are heuristics and not magic.
fn has_magic(mime: &str) -> bool {
    if mime.starts_with("text/") {
        return false;
    }
    match container(mime) {
        Some(XML) => false,
        Some(_) => true,
        None => infer::is_mime_supported(mime),
    }
}

/// Collapse the spellings of one format onto one. Only spellings: a
/// format the sniffer reports as another (HEIC as HEIF, WOFF2 as WOFF) is
/// a variant, [`base`]'s, and a format packaged in another (an APK in a
/// ZIP) is [`container`]'s. The allowlist and the category are on the
/// canonical spelling, so what is collapsed here is what a list entry
/// admits.
pub fn canonical(mime: &str) -> &str {
    match mime {
        // S3's spelling, which SDKs pass on.
        "binary/octet-stream" => OCTET_STREAM,
        "image/jpg" | "image/pjpeg" => "image/jpeg",
        "image/x-icon" | "image/icon" => "image/vnd.microsoft.icon",
        "image/x-ms-bmp" => "image/bmp",
        "image/x-tiff" => "image/tiff",
        "audio/mp3" | "audio/x-mpeg" | "audio/mpeg3" => "audio/mpeg",
        "audio/wav" | "audio/wave" | "audio/vnd.wave" => "audio/x-wav",
        "audio/mp4" | "audio/x-m4a" => "audio/m4a",
        "audio/flac" => "audio/x-flac",
        "audio/aiff" => "audio/x-aiff",
        "video/x-m4v" => "video/mp4",
        "video/avi" | "video/msvideo" => "video/x-msvideo",
        "application/x-zip-compressed" => "application/zip",
        "application/x-rar-compressed" | "application/x-rar" => "application/vnd.rar",
        "application/x-gzip" => "application/gzip",
        "application/x-bzip" => "application/x-bzip2",
        "application/x-7z" => "application/x-7z-compressed",
        "application/xml" => "text/xml",
        "application/x-pdf" => "application/pdf",
        "application/x-msword" => "application/msword",
        "text/rtf" => "application/rtf",
        "application/x-sqlite3" => "application/vnd.sqlite3",
        // The names `infer` reports fonts under predate RFC 8081.
        "font/sfnt" => "application/font-sfnt",
        "font/woff" => "application/font-woff",
        "application/x-font-ttf" => "font/ttf",
        "application/x-font-opentype" => "font/otf",
        "application/x-java-archive" => "application/java-archive",
        other => other,
    }
}

/// The format the sniffer reports for a variant it cannot tell apart from
/// it: a macro-enabled or template Office Open XML file sniffs as the
/// plain document, workbook or presentation, an OpenDocument template or
/// master as its document, a HEIC image as HEIF, a TrueType or OpenType
/// font as the SFNT container, a WOFF2 font as WOFF. The declared variant
/// is then checked as that format, so it is compatible with what its base
/// is compatible with and a sibling of what its base is a sibling of. A
/// variant is not an alias: a list that names the base does not admit it,
/// and the renderer plans it as its base.
pub fn base(mime: &str) -> Option<&'static str> {
    const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
    const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
    const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
    match mime {
        "image/heic" | "image/heic-sequence" | "image/heif-sequence" => Some("image/heif"),
        "font/ttf" | "font/otf" => Some("application/font-sfnt"),
        "font/woff2" => Some("application/font-woff"),
        "application/vnd.ms-word.document.macroenabled.12"
        | "application/vnd.openxmlformats-officedocument.wordprocessingml.template"
        | "application/vnd.ms-word.template.macroenabled.12" => Some(DOCX),
        "application/vnd.ms-excel.sheet.macroenabled.12"
        | "application/vnd.ms-excel.sheet.binary.macroenabled.12"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.template"
        | "application/vnd.ms-excel.template.macroenabled.12"
        | "application/vnd.ms-excel.addin.macroenabled.12" => Some(XLSX),
        "application/vnd.ms-powerpoint.presentation.macroenabled.12"
        | "application/vnd.openxmlformats-officedocument.presentationml.slideshow"
        | "application/vnd.ms-powerpoint.slideshow.macroenabled.12"
        | "application/vnd.openxmlformats-officedocument.presentationml.template"
        | "application/vnd.ms-powerpoint.template.macroenabled.12"
        | "application/vnd.ms-powerpoint.addin.macroenabled.12" => Some(PPTX),
        "application/vnd.oasis.opendocument.text-template"
        | "application/vnd.oasis.opendocument.text-master"
        | "application/vnd.oasis.opendocument.text-master-template"
        | "application/vnd.oasis.opendocument.text-web" => {
            Some("application/vnd.oasis.opendocument.text")
        }
        "application/vnd.oasis.opendocument.spreadsheet-template" => {
            Some("application/vnd.oasis.opendocument.spreadsheet")
        }
        "application/vnd.oasis.opendocument.presentation-template" => {
            Some("application/vnd.oasis.opendocument.presentation")
        }
        _ => None,
    }
}

/// The generic format a specific one is packaged in.
pub(crate) fn container(mime: &str) -> Option<&'static str> {
    match mime {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.oasis.opendocument.text"
        | "application/vnd.oasis.opendocument.spreadsheet"
        | "application/vnd.oasis.opendocument.presentation"
        | "application/vnd.oasis.opendocument.graphics"
        | "application/vnd.oasis.opendocument.graphics-template"
        | "application/vnd.oasis.opendocument.chart"
        | "application/vnd.oasis.opendocument.formula"
        | "application/vnd.oasis.opendocument.image"
        | "application/vnd.oasis.opendocument.database"
        | "application/vnd.sun.xml.writer"
        | "application/vnd.sun.xml.calc"
        | "application/vnd.sun.xml.impress"
        | "application/vnd.sun.xml.draw"
        | "application/vnd.apple.pages"
        | "application/vnd.apple.numbers"
        | "application/vnd.apple.keynote"
        | "application/x-iwork-pages-sffpages"
        | "application/x-iwork-numbers-sffnumbers"
        | "application/x-iwork-keynote-sffkey"
        | "application/vnd.ms-xpsdocument"
        | "application/oxps"
        | "application/vnd.google-earth.kmz"
        | "application/epub+zip"
        | "application/java-archive"
        | "application/vnd.android.package-archive"
        | "application/x-google-chrome-extension"
        | "image/openraster" => Some(ZIP),
        "application/msword"
        | "application/vnd.ms-excel"
        | "application/vnd.ms-powerpoint"
        | "application/vnd.ms-outlook"
        | "application/vnd.ms-publisher"
        | "application/vnd.visio" => Some(OLE),
        "image/svg+xml"
        | "application/xhtml+xml"
        | "application/rss+xml"
        | "application/atom+xml" => Some(XML),
        other if other.ends_with("+xml") => Some(XML),
        _ => None,
    }
}

/// Bytes of a file looked at to pick its loader.
pub const LOADER_SNIFF_LEN: usize = 1024;

/// The vips loader for `head`, by magic, or `None` when the bytes are not
/// something vips may be handed. vips chooses its loader by content, not by
/// the declared type, and the long tail of that choice ends at ImageMagick,
/// so the allowlist of renderable types is enforced again here, on the
/// bytes, for every file that reaches the renderer. PNM is here for the
/// pages the helpers write, and `.v` for the map `vips composite` writes.
/// BMP is not: libvips has no loader of its own for it, and would hand it
/// to ImageMagick.
pub fn loader_for(head: &[u8]) -> Option<&'static str> {
    match infer::get(head).map(|kind| kind.mime_type()) {
        Some("image/jpeg") => return Some("jpeg"),
        Some("image/png") => return Some("png"),
        Some("image/gif") => return Some("gif"),
        Some("image/webp") => return Some("webp"),
        Some("image/tiff") => return Some("tiff"),
        Some("image/heif" | "image/avif") => return Some("heif"),
        Some("image/jp2") => return Some("jp2k"),
        Some("application/pdf") => return Some("pdf"),
        // Text sniffers fire on XML, so an SVG falls through to its own
        // check below.
        _ => {}
    }
    // A bare JPEG 2000 codestream, which is how PDFs embed them.
    if head.starts_with(&[0xff, 0x4f, 0xff, 0x51]) {
        return Some("jp2k");
    }
    if matches!(head, [b'P', b'4'..=b'6', b' ' | b'\t' | b'\n' | b'\r', ..]) {
        return Some("pnm");
    }
    // The native format, in either byte order.
    if head.starts_with(&[0xb6, 0xa6, 0xf2, 0x08]) || head.starts_with(&[0x08, 0xf2, 0xa6, 0xb6]) {
        return Some("vips");
    }
    // vips looks for the root element in the first KiB; so does this.
    if head[..head.len().min(LOADER_SNIFF_LEN)]
        .windows(4)
        .any(|w| w == b"<svg")
    {
        return Some("svg");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loaders_by_magic() {
        assert_eq!(
            loader_for(&[0xff, 0xd8, 0xff, 0xe0, 0, 0x10, b'J', b'F']),
            Some("jpeg")
        );
        assert_eq!(loader_for(b"%PDF-1.4\n"), Some("pdf"));
        assert_eq!(loader_for(b"P5\n4 4\n255\n"), Some("pnm"));
        assert_eq!(loader_for(b"P7\n"), None);
        assert_eq!(
            loader_for(&[0xb6, 0xa6, 0xf2, 0x08, 0x2c, 0x01]),
            Some("vips")
        );
        assert_eq!(loader_for(&[0x08, 0xf2, 0xa6, 0xb6, 0, 0]), Some("vips"));
        assert_eq!(
            loader_for(b"<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
            Some("svg")
        );
        assert_eq!(loader_for(&[0xff, 0x4f, 0xff, 0x51, 0, 0x2f]), Some("jp2k"));
        // Known to infer, not to the allowlist: a camera raw file.
        assert_eq!(loader_for(b"II*\0\x10\0\0\0CR\x02\0"), None);
        // Known to infer, loadable by vips, but only through ImageMagick:
        // a BMP.
        assert_eq!(loader_for(b"BM:\x01\0\0\0\0\0\0z\0\0\0l\0\0\0"), None);
        // Unknown to infer, loadable by ImageMagick.
        assert_eq!(loader_for(b"/* XPM */\nstatic char *x[] = {"), None);
        assert_eq!(loader_for(b""), None);
        assert_eq!(loader_for(b"hello"), None);
    }

    #[test]
    fn sniffs_common_formats() {
        assert_eq!(
            sniff(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
            Some("image/png")
        );
        assert_eq!(sniff(b"%PDF-1.4\n"), Some("application/pdf"));
        assert_eq!(sniff(b"hello, world"), None);
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(br#"{"a": 1}"#), None);
        assert_eq!(
            sniff(br#"{"type": "FeatureCollection", "features": []}"#),
            Some(GEOJSON)
        );
        assert_eq!(
            sniff(b"<?xml version=\"1.0\"?>\n<gpx version=\"1.1\">"),
            Some(GPX)
        );
        assert_eq!(sniff(b"<gpx version=\"1.1\">"), Some(GPX));
        assert_eq!(sniff(b"<?xml version=\"1.0\"?>\n<kml>"), Some(KML));
        assert_eq!(sniff(b"<kml>"), Some(KML));
        assert_eq!(sniff(b"<?xml version=\"1.0\"?>\n<svg>"), Some("text/xml"));
        assert_eq!(sniff(b"<svg>"), None);
        assert_eq!(sniff(&zip_head(b"doc.kml")), Some(KMZ));
        assert_eq!(sniff(&zip_head(b"files/icon.png")), Some("application/zip"));
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
    fn refines_json_declarations_to_geojson() {
        for declared in ["application/json", "text/json", "text/plain"] {
            assert!(compatible(declared, GEOJSON), "{declared}");
            assert_eq!(refine(declared, GEOJSON), Some(GEOJSON), "{declared}");
        }
        assert!(compatible(GEOJSON, GEOJSON));
        assert_eq!(refine(GEOJSON, GEOJSON), None);
        assert!(compatible("text/csv", GEOJSON));
        assert_eq!(refine("text/csv", GEOJSON), None);
        assert!(compatible("application/octet-stream", GEOJSON));
        assert_eq!(refine("application/octet-stream", GEOJSON), None);
        assert!(!compatible("image/png", GEOJSON));
        assert!(!compatible("application/pdf", GEOJSON));
        assert_eq!(refine("application/json", "image/png"), None);
    }

    #[test]
    fn refines_xml_declarations_to_gpx() {
        for declared in ["application/xml", "text/xml", "text/plain"] {
            assert!(compatible(declared, GPX), "{declared}");
            assert_eq!(refine(declared, GPX), Some(GPX), "{declared}");
        }
        assert!(compatible(GPX, GPX));
        assert_eq!(refine(GPX, GPX), None);
        // A declared GPX whose root element sits past the sniffed head.
        assert!(compatible(GPX, "text/xml"));
        assert_eq!(refine(GPX, "text/xml"), None);
        assert!(compatible("text/csv", GPX));
        assert_eq!(refine("text/csv", GPX), None);
        assert!(compatible("application/octet-stream", GPX));
        assert_eq!(refine("application/octet-stream", GPX), None);
        assert!(!compatible("image/png", GPX));
        assert!(!compatible("application/json", GPX));
        assert!(!compatible(GEOJSON, GPX));
        assert!(!compatible(GPX, GEOJSON));
        assert_eq!(refine("application/xml", "text/xml"), None);
    }

    #[test]
    fn refines_xml_declarations_to_kml() {
        for declared in ["application/xml", "text/xml", "text/plain"] {
            assert!(compatible(declared, KML), "{declared}");
            assert_eq!(refine(declared, KML), Some(KML), "{declared}");
        }
        assert!(compatible(KML, KML));
        assert_eq!(refine(KML, KML), None);
        // A declared KML whose root element sits past the sniffed head.
        assert!(compatible(KML, "text/xml"));
        assert_eq!(refine(KML, "text/xml"), None);
        assert!(compatible("text/csv", KML));
        assert_eq!(refine("text/csv", KML), None);
        assert!(compatible("application/octet-stream", KML));
        assert!(!compatible("image/png", KML));
        assert!(!compatible("application/json", KML));
        assert!(!compatible(GPX, KML));
        assert!(!compatible(KML, GPX));
        assert!(!compatible(KMZ, KML));
    }

    #[test]
    fn refines_zip_declarations_to_kmz() {
        for declared in ["application/zip", "application/x-zip-compressed"] {
            assert!(compatible(declared, KMZ), "{declared}");
            assert_eq!(refine(declared, KMZ), Some(KMZ), "{declared}");
        }
        assert!(compatible(KMZ, KMZ));
        assert_eq!(refine(KMZ, KMZ), None);
        // A declared KMZ whose first entry is an icon sniffs as a ZIP.
        assert!(compatible(KMZ, "application/zip"));
        assert_eq!(refine(KMZ, "application/zip"), None);
        assert!(compatible("application/octet-stream", KMZ));
        assert_eq!(refine("application/octet-stream", KMZ), None);
        // A ZIP-packaged sibling is not a KMZ.
        let docx = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
        assert!(!compatible(docx, KMZ));
        assert!(!compatible(KMZ, docx));
        assert!(!compatible("application/pdf", KMZ));
        assert!(!compatible(KML, KMZ));
        assert!(!compatible("text/plain", KMZ));
        assert_eq!(refine(docx, KMZ), None);
    }

    #[test]
    fn compatibility_rules() {
        assert!(compatible("image/jpeg", "image/jpeg"));
        assert!(compatible("image/jpg", "image/jpeg"));
        assert!(compatible("image/heic", "image/heif"));
        assert!(!compatible("image/jpeg", "image/png"));
        assert!(!compatible("image/jpeg", "application/pdf"));

        // Containers in either direction, but not siblings.
        let docx = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
        let xlsx = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
        assert!(compatible(docx, "application/zip"));
        assert!(compatible("application/zip", docx));
        assert!(compatible("application/x-zip-compressed", docx));
        assert!(!compatible(docx, xlsx));
        assert!(compatible(
            "application/msword",
            "application/x-ole-storage"
        ));
        assert!(!compatible("application/msword", "application/pdf"));

        assert!(!compatible("text/plain", "image/png"));

        assert!(compatible("application/octet-stream", "image/png"));
        assert!(compatible("binary/octet-stream", "image/png"));
        assert!(compatible("text/rtf", "application/rtf"));
    }

    /// What the sniffer reports a format as is not a spelling of it: the
    /// declared variant is checked as its base, and nothing else is.
    #[test]
    fn families_the_sniffer_cannot_tell_apart() {
        assert!(compatible("image/heic", "image/heif"));
        assert!(compatible("image/heic-sequence", "image/heif"));
        assert!(compatible("image/heif", "image/heif"));
        assert!(!compatible("image/heic", "image/png"));
        assert!(compatible("font/ttf", "application/font-sfnt"));
        assert!(compatible("font/otf", "application/font-sfnt"));
        assert!(compatible(
            "application/x-font-ttf",
            "application/font-sfnt"
        ));
        assert!(compatible("font/sfnt", "application/font-sfnt"));
        assert!(compatible("font/woff", "application/font-woff"));
        assert!(compatible("font/woff2", "application/font-woff"));
        assert!(!compatible("font/woff2", "application/font-sfnt"));
        // Packaged in a ZIP, like a DOCX.
        assert!(compatible(
            "application/vnd.android.package-archive",
            "application/zip"
        ));
        assert!(!compatible(
            "application/vnd.android.package-archive",
            "application/pdf"
        ));
        // A JAR is a ZIP; `application/java`, which `infer` reports for a
        // class file, is neither a JAR nor a ZIP.
        assert_eq!(
            sniff(b"\xca\xfe\xba\xbe\x00\x00\x00\x41\x00\x10"),
            Some("application/java")
        );
        assert!(compatible("application/java-archive", "application/zip"));
        assert!(compatible("application/x-java-archive", "application/zip"));
        assert!(!compatible("application/java-archive", "application/java"));
        assert!(!compatible("application/zip", "application/java"));
        // `infer` tells a Matroska file from a WebM one by its doctype, so
        // the one is not the other.
        assert!(compatible("video/x-matroska", "video/x-matroska"));
        assert!(!compatible("video/webm", "video/x-matroska"));
        assert!(!compatible("video/x-matroska", "video/webm"));
        for (variant, base) in [
            ("image/heic", "image/heif"),
            ("font/ttf", "application/font-sfnt"),
            ("font/woff2", "application/font-woff"),
            ("application/vnd.android.package-archive", "application/zip"),
            ("video/x-matroska", "video/webm"),
        ] {
            assert_ne!(canonical(variant), base, "{variant} is not a spelling");
        }
    }

    /// The text sniffers are heuristics on the first bytes: they
    /// contradict a declaration only when its real bytes would have
    /// sniffed as something, so a type with magic of its own.
    #[test]
    fn text_heuristics_contradict_only_magic() {
        // What the heuristics fire on.
        assert_eq!(
            sniff(b"<!-- icon -->\n<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
            Some("text/html")
        );
        assert_eq!(sniff(b"<!DOCTYPE html>\n<html/>"), Some("text/html"));
        assert_eq!(sniff(b"#!/bin/sh\necho hi\n"), Some("text/x-shellscript"));
        assert_eq!(sniff(b"<?xml version=\"1.0\"?>\n<svg/>"), Some("text/xml"));

        // Text, and the types that open on text: no magic to contradict.
        for (declared, sniffed) in [
            ("text/plain", "text/html"),
            ("text/csv", "text/x-shellscript"),
            ("text/html", "text/x-shellscript"),
            ("image/svg+xml", "text/xml"),
            ("image/svg+xml", "text/html"),
            ("application/xml", "text/xml"),
            ("application/xhtml+xml", "text/html"),
            ("application/gpx+xml", "text/html"),
            ("application/vnd.google-earth.kml+xml", "text/html"),
            ("application/json", "text/html"),
            ("application/x-sh", "text/x-shellscript"),
            ("application/x-shellscript", "text/x-shellscript"),
            ("application/javascript", "text/x-shellscript"),
            ("application/x-python", "text/x-shellscript"),
            ("application/octet-stream", "text/html"),
        ] {
            assert!(compatible(declared, sniffed), "{declared} as {sniffed}");
        }

        // A type with magic, `infer`'s or a package's, is another kind of
        // file when its bytes sniff as text.
        let docx = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
        for (declared, sniffed) in [
            ("image/png", "text/html"),
            ("image/jpg", "text/xml"),
            ("audio/mpeg", "text/x-shellscript"),
            ("video/mp4", "text/html"),
            ("font/ttf", "text/x-shellscript"),
            ("application/pdf", "text/x-shellscript"),
            ("application/zip", "text/html"),
            ("application/gzip", "text/html"),
            ("application/rtf", "text/html"),
            (docx, "text/html"),
            ("application/vnd.ms-excel.sheet.macroenabled.12", "text/xml"),
            ("application/msword", "text/html"),
            // Packaged in a ZIP, unknown to `infer`.
            ("application/vnd.oasis.opendocument.graphics", "text/html"),
            ("application/vnd.google-earth.kmz", "text/xml"),
        ] {
            assert!(!compatible(declared, sniffed), "{declared} as {sniffed}");
        }
    }

    #[test]
    fn variants_are_checked_as_their_base_format() {
        const ZIP: &str = "application/zip";
        let docx = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
        let xlsx = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
        let pptx = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let odt = "application/vnd.oasis.opendocument.text";
        let ods = "application/vnd.oasis.opendocument.spreadsheet";
        let odp = "application/vnd.oasis.opendocument.presentation";
        for (variant, sniffed, sibling) in [
            (
                "application/vnd.ms-word.document.macroenabled.12",
                docx,
                xlsx,
            ),
            (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.template",
                docx,
                pptx,
            ),
            ("application/vnd.ms-excel.sheet.macroenabled.12", xlsx, docx),
            (
                "application/vnd.ms-excel.sheet.binary.macroenabled.12",
                xlsx,
                docx,
            ),
            (
                "application/vnd.openxmlformats-officedocument.spreadsheetml.template",
                xlsx,
                pptx,
            ),
            (
                "application/vnd.openxmlformats-officedocument.presentationml.slideshow",
                pptx,
                docx,
            ),
            (
                "application/vnd.ms-powerpoint.presentation.macroenabled.12",
                pptx,
                xlsx,
            ),
            ("application/vnd.oasis.opendocument.text-template", odt, ods),
            ("application/vnd.oasis.opendocument.text-master", odt, odp),
            (
                "application/vnd.oasis.opendocument.spreadsheet-template",
                ods,
                odt,
            ),
            (
                "application/vnd.oasis.opendocument.presentation-template",
                odp,
                ods,
            ),
        ] {
            assert!(compatible(variant, sniffed), "{variant} as {sniffed}");
            // The sniffer may only get as far as the ZIP.
            assert!(compatible(variant, ZIP), "{variant} as zip");
            assert!(!compatible(variant, sibling), "{variant} as {sibling}");
            assert!(!compatible(variant, "application/pdf"), "{variant} as pdf");
        }
    }

    #[test]
    fn zip_and_ole_packaged_formats() {
        for zipped in [
            "application/vnd.apple.pages",
            "application/vnd.apple.numbers",
            "application/vnd.apple.keynote",
            "application/x-iwork-pages-sffpages",
            "application/vnd.ms-xpsdocument",
            "application/oxps",
            "application/vnd.sun.xml.writer",
            "application/vnd.oasis.opendocument.graphics-template",
            "application/vnd.google-earth.kmz",
        ] {
            assert!(compatible(zipped, "application/zip"), "{zipped}");
            assert!(!compatible(zipped, "application/pdf"), "{zipped}");
        }
        assert!(compatible(
            "application/vnd.ms-publisher",
            "application/x-ole-storage"
        ));
        assert!(!compatible(
            "application/vnd.ms-publisher",
            "application/zip"
        ));
    }

    #[test]
    fn any_ole_file_sniffs_as_the_storage() {
        let mut head = vec![0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
        head.resize(SNIFF_LEN, 0);
        // Without its `std` feature `infer` calls every OLE signature a
        // Word document; with it, it parses the directory in-process with
        // `cfb`, a parser on hostile bytes, which fails on this head and
        // falls through to the MSI matcher. The feature is off in
        // `Cargo.toml`, and unification from another crate in the
        // workspace would switch it on silently: this is the alarm.
        assert_eq!(
            infer::get(&head).map(|kind| kind.mime_type()),
            Some("application/msword"),
            "infer's `std` feature is on: an OLE file is parsed in-process"
        );
        assert_eq!(sniff(&head), Some(OLE));
        for declared in [
            "application/msword",
            "application/vnd.ms-excel",
            "application/vnd.ms-powerpoint",
            "application/vnd.ms-outlook",
            "application/vnd.visio",
            "application/vnd.ms-publisher",
            OLE,
            "application/octet-stream",
        ] {
            assert!(compatible(declared, OLE), "{declared}");
        }
        assert!(!compatible("application/pdf", OLE));
    }
}
