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

/// Bytes needed before a verdict. PDF allows its magic anywhere in the first
/// 1 KiB; ZIP-based office formats need a few local file headers.
pub const SNIFF_LEN: usize = 8 * 1024;

/// What the first bytes of an upload look like, if recognisable. JSON has
/// no magic, but a JSON document that opens on a Feature or a
/// FeatureCollection is a GeoJSON one, and is sniffed as such so that a
/// `.geojson` declared as JSON or text can be refined (see [`refine`]).
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    if let Some(kind) = infer::get(head) {
        return Some(kind.mime_type());
    }
    geojson::looks_like_geojson(head).then_some(GEOJSON)
}

/// The content type a blob declared as `declared` gets once its bytes
/// sniffed as `sniffed`: the sniffed type when it is more precise than a
/// declaration that covers it, as a GeoJSON file declared as JSON or plain
/// text; `None` to keep the declaration. The category is the same either
/// way.
pub fn refine(declared: &str, sniffed: &str) -> Option<&'static str> {
    match (canonical(declared), sniffed) {
        ("application/json" | "text/json" | "text/plain", GEOJSON) => Some(GEOJSON),
        _ => None,
    }
}

/// Whether bytes sniffed as `sniffed` may be served as `declared`.
pub fn compatible(declared: &str, sniffed: &str) -> bool {
    if declared == "application/octet-stream" {
        return true;
    }
    let d = canonical(declared);
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
    // Text sniffers (HTML, shell scripts, XML) are heuristics; any text
    // declaration covers them.
    d.starts_with("text/") && s.starts_with("text/")
}

/// Collapse aliases onto one spelling.
pub fn canonical(mime: &str) -> &str {
    match mime {
        "image/jpg" | "image/pjpeg" => "image/jpeg",
        "image/heic" | "image/heic-sequence" | "image/heif-sequence" => "image/heif",
        "image/x-icon" | "image/icon" => "image/vnd.microsoft.icon",
        "image/x-ms-bmp" => "image/bmp",
        "image/x-tiff" => "image/tiff",
        "audio/mp3" | "audio/x-mpeg" | "audio/mpeg3" => "audio/mpeg",
        "audio/wav" | "audio/wave" | "audio/vnd.wave" => "audio/x-wav",
        "audio/mp4" | "audio/x-m4a" => "audio/m4a",
        "audio/flac" => "audio/x-flac",
        "audio/aiff" => "audio/x-aiff",
        "video/x-m4v" => "video/mp4",
        "video/x-matroska" => "video/webm",
        "video/avi" | "video/msvideo" => "video/x-msvideo",
        "application/x-zip-compressed" => "application/zip",
        "application/x-rar-compressed" | "application/x-rar" => "application/vnd.rar",
        "application/x-gzip" => "application/gzip",
        "application/x-bzip" => "application/x-bzip2",
        "application/x-7z" => "application/x-7z-compressed",
        "application/xml" => "text/xml",
        "application/x-pdf" => "application/pdf",
        "application/x-msword" => "application/msword",
        "application/x-sqlite3" => "application/vnd.sqlite3",
        "font/ttf"
        | "font/otf"
        | "font/sfnt"
        | "application/x-font-ttf"
        | "application/x-font-opentype" => "application/font-sfnt",
        "font/woff" | "font/woff2" => "application/font-woff",
        "application/java-archive" | "application/x-java-archive" => "application/java",
        "application/vnd.android.package-archive" => "application/zip",
        other => other,
    }
}

/// The generic format a specific one is packaged in.
fn container(mime: &str) -> Option<&'static str> {
    const ZIP: &str = "application/zip";
    const OLE: &str = "application/x-ole-storage";
    const XML: &str = "text/xml";
    match mime {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.oasis.opendocument.text"
        | "application/vnd.oasis.opendocument.spreadsheet"
        | "application/vnd.oasis.opendocument.presentation"
        | "application/vnd.oasis.opendocument.graphics"
        | "application/epub+zip"
        | "application/java"
        | "application/x-google-chrome-extension"
        | "image/openraster" => Some(ZIP),
        "application/msword"
        | "application/vnd.ms-excel"
        | "application/vnd.ms-powerpoint"
        | "application/vnd.ms-outlook"
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
pub fn loader_for(head: &[u8]) -> Option<&'static str> {
    match infer::get(head).map(|kind| kind.mime_type()) {
        Some("image/jpeg") => return Some("jpeg"),
        Some("image/png") => return Some("png"),
        Some("image/gif") => return Some("gif"),
        Some("image/webp") => return Some("webp"),
        Some("image/tiff") => return Some("tiff"),
        Some("image/bmp") => return Some("bmp"),
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

        // Text is never rejected on a text heuristic.
        assert!(compatible("text/plain", "text/html"));
        assert!(compatible("text/csv", "text/x-shellscript"));
        assert!(compatible("image/svg+xml", "text/xml"));
        assert!(compatible("application/xml", "text/xml"));
        assert!(!compatible("text/plain", "image/png"));

        assert!(compatible("application/octet-stream", "image/png"));
        assert!(compatible("font/ttf", "application/font-sfnt"));
    }
}
