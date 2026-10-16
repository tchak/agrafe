//! Pulling the one image out of a document that is really a wrapped image:
//! a PDF whose single page is one picture, a DOCX or ODT holding one media
//! file and next to no text. Users answer "send a screenshot" with such
//! files, and the embedded image at full resolution is a better preview
//! than a render of the page around it.
//!
//! This code parses untrusted containers, so it runs in the `agrafe
//! extract` helper, a sandboxed subprocess, never in the server. Every
//! decode is bounded by `max_output`, and anything doubtful answers `None`:
//! the caller then falls back to rendering the page, or to no preview.

pub mod office;
pub mod pdf;

use crate::classify;

/// What makes a document "really an image". From `media.embedded_image`.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// Fraction of the page the image must cover (PDF only).
    pub min_coverage: f32,
    /// Non-whitespace characters of text allowed around it.
    pub max_text_chars: u32,
}

/// An image pulled out of a document, in a format libvips loads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    pub bytes: Vec<u8>,
    /// `jpeg`, `png`, `ppm`, ... Informational: vips sniffs the bytes.
    pub format: &'static str,
}

pub fn extract(
    content_type: &str,
    bytes: &[u8],
    thresholds: &Thresholds,
    max_output: usize,
) -> Option<Extracted> {
    match content_type {
        classify::PDF => pdf::extract(bytes, thresholds, max_output),
        super::DOCX => office::extract_docx(bytes, thresholds, max_output),
        super::ODT => office::extract_odt(bytes, thresholds, max_output),
        _ => None,
    }
}

/// The format name for raster bytes libvips can load, by magic.
pub(crate) fn raster_format(bytes: &[u8]) -> Option<&'static str> {
    Some(match infer::get(bytes)?.mime_type() {
        "image/jpeg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/tiff" => "tiff",
        "image/bmp" => "bmp",
        "image/heif" | "image/heic" | "image/avif" => "heif",
        "image/jp2" | "image/jpx" => "jp2",
        _ => return None,
    })
}
