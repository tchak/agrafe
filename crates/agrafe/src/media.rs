//! Derived representations: which files can be rendered, from what, and the
//! tools that do it. Decoding and rendering never happen in this process;
//! `sandbox` runs them as subprocesses with resource limits.

pub mod extract;
pub mod icons;
pub mod render;
pub mod sandbox;
pub mod text;

use crate::classify::{self, Category};
use crate::sniff;

pub const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
pub const ODT: &str = "application/vnd.oasis.opendocument.text";

/// Where a representation of a file can come from. A file lists its sources
/// in the order they are tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The file is an image: decode and transcode it.
    Image,
    /// A document that wraps one image with next to no text: pull the image
    /// out at full resolution.
    EmbeddedImage,
    /// Render the first page.
    FirstPage,
}

/// The preview sources for a content type, in order; the thumbnail is
/// derived from the preview. Empty when the file cannot be rendered, in
/// which case there is no preview and the thumbnail is a text sample or an
/// icon. Image types are an allowlist of what libvips loads natively:
/// everything else would go through ImageMagick, which is more attack
/// surface than it is worth.
pub fn sources(content_type: &str) -> &'static [Source] {
    match sniff::canonical(content_type) {
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "image/tiff" | "image/heif"
        | "image/avif" | "image/svg+xml" | "image/bmp" | "image/jp2" => &[Source::Image],
        classify::PDF => &[Source::EmbeddedImage, Source::FirstPage],
        DOCX | ODT => &[Source::EmbeddedImage],
        _ => &[],
    }
}

/// Whether a file with no preview gets a thumbnail of its first lines set
/// in a monospace font: the `text` category, plus CSV and TSV, which
/// classify as spreadsheets. A hint of what the file is, not a way to read
/// it, which is why there is no text preview.
pub fn text_sample(content_type: &str) -> bool {
    let canonical = sniff::canonical(content_type);
    matches!(canonical, classify::CSV | "text/tab-separated-values")
        || classify::classify(canonical) == Category::Text
}

/// Bytes of a file looked at to pick its loader.
pub const SNIFF_LEN: usize = 1024;

/// The vips loader for `head`, by magic, or `None` when the bytes are not
/// something vips may be handed. vips chooses its loader by content, not by
/// the declared type, and the long tail of that choice ends at ImageMagick,
/// so the allowlist of `sources` is enforced again here, on the bytes, for
/// every file that reaches it. PNM is here for the pages the helpers write.
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
    // vips looks for the root element in the first KiB; so does this.
    if head[..head.len().min(SNIFF_LEN)]
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
    fn sources_by_type() {
        assert_eq!(sources("image/jpg"), &[Source::Image]);
        assert_eq!(sources("image/heic"), &[Source::Image]);
        assert_eq!(
            sources(classify::PDF),
            &[Source::EmbeddedImage, Source::FirstPage]
        );
        assert_eq!(sources(DOCX), &[Source::EmbeddedImage]);
        assert!(sources("image/vnd.adobe.photoshop").is_empty());
        assert!(sources("text/plain").is_empty());
        assert!(sources(classify::XLSX).is_empty());
        for text in [
            "text/plain",
            "application/json",
            classify::CSV,
            "text/tab-separated-values",
        ] {
            assert!(text_sample(text), "{text}");
        }
        for other in [
            classify::XLSX,
            classify::PDF,
            "image/png",
            "application/octet-stream",
        ] {
            assert!(!text_sample(other), "{other}");
        }
    }
}
