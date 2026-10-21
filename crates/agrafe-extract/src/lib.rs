//! Pulling the one image out of a document that is really a wrapped image:
//! a PDF whose single page is one picture, a Writer package (a DOCX or ODT,
//! or one of their macro-enabled, template and master variants) holding
//! one media file and next to no text. Users answer "send a screenshot"
//! with such files, and the embedded image at full resolution is a better
//! preview than a render of the page around it.
//!
//! This code parses untrusted containers, so it runs in the
//! `agrafe-extract` helper, a sandboxed subprocess, never in the server:
//! the server does not even link this crate. Every decode is bounded by
//! `max_output`, and anything doubtful answers `None`: the caller then falls
//! back to rendering the page, or to no preview.

mod office;
mod pdf;

use agrafe_types::classify;
use agrafe_types::office::{Application, Package};

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
    /// The encoded image as the document stores it, or a PNM wrapped
    /// around the raw samples of a PDF image.
    pub bytes: Vec<u8>,
    /// `jpeg`, `png`, `ppm`, ... Informational: vips sniffs the bytes.
    pub format: &'static str,
}

/// The one image a PDF or a Writer package wraps, if it is really an
/// image by `thresholds` and fits in `max_output` bytes; `None` otherwise,
/// and for any other content type. The Office Open XML and OpenDocument
/// variants of a Writer document (`agrafe_types::office`) are the same
/// package as the DOCX or ODT they derive from, with the same parts; the
/// binary formats and RTF are not opened.
pub fn extract(
    content_type: &str,
    bytes: &[u8],
    thresholds: &Thresholds,
    max_output: usize,
) -> Option<Extracted> {
    if content_type == classify::PDF {
        return pdf::extract(bytes, thresholds, max_output);
    }
    let format = agrafe_types::office::format(content_type)?;
    match (format.application, format.package) {
        (Application::Writer, Package::Ooxml) => {
            office::extract_docx(bytes, thresholds, max_output)
        }
        (Application::Writer, Package::Odf) => office::extract_odt(bytes, thresholds, max_output),
        _ => None,
    }
}

/// The format of raster bytes libvips may load, by magic: the renderer's
/// allowlist minus the vector formats, which are not "a picture inside a
/// document".
pub(crate) fn raster_format(bytes: &[u8]) -> Option<&'static str> {
    agrafe_types::sniff::loader_for(bytes).filter(|format| !matches!(*format, "pdf" | "svg"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agrafe_types::office::*;

    const THRESHOLDS: Thresholds = Thresholds {
        min_coverage: 0.7,
        max_text_chars: 50,
    };
    const LIMIT: usize = 1 << 20;

    /// A Writer variant opens as the package it derives from; the formats
    /// with no extractor answer `None` whatever the bytes.
    #[test]
    fn dispatches_by_format() {
        let docx = include_bytes!("../../../fixtures/image.docx");
        let odt = include_bytes!("../../../fixtures/image.odt");
        for writer in [DOCX, DOCM, DOTX, DOTM] {
            assert!(
                extract(writer, docx, &THRESHOLDS, LIMIT).is_some(),
                "{writer}"
            );
        }
        for writer in [ODT, OTT, ODM] {
            assert!(
                extract(writer, odt, &THRESHOLDS, LIMIT).is_some(),
                "{writer}"
            );
        }
        for other in [DOC, RTF, XLSX, XLSM, PPTX, ODP, ODG, "application/zip"] {
            assert_eq!(extract(other, docx, &THRESHOLDS, LIMIT), None, "{other}");
            assert_eq!(extract(other, odt, &THRESHOLDS, LIMIT), None, "{other}");
        }
    }
}
