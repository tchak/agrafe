//! Magic-byte sniffing of uploads, compared against the declared content type.
//!
//! The check is deliberately lenient where the declared type is a container
//! of the sniffed one (a DOCX is a ZIP), where the two are aliases
//! (`image/jpg`), and for text, which has no magic. It only rejects what is
//! clearly another kind of file.

/// Bytes needed before a verdict. PDF allows its magic anywhere in the first
/// 1 KiB; ZIP-based office formats need a few local file headers.
pub const SNIFF_LEN: usize = 8 * 1024;

/// What the first bytes of an upload look like, if recognisable.
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    infer::get(head).map(|t| t.mime_type())
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
    // Text sniffers (HTML, shell scripts, XML) are heuristics; any text
    // declaration covers them.
    d.starts_with("text/") && s.starts_with("text/")
}

/// Collapse aliases onto one spelling.
fn canonical(mime: &str) -> &str {
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
        | "application/atom+xml"
        | "application/geo+json" => Some(XML),
        other if other.ends_with("+xml") => Some(XML),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_common_formats() {
        assert_eq!(
            sniff(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
            Some("image/png")
        );
        assert_eq!(sniff(b"%PDF-1.4\n"), Some("application/pdf"));
        assert_eq!(sniff(b"hello, world"), None);
        assert_eq!(sniff(b""), None);
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
