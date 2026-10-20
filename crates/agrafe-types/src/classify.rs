//! Content type to category mapping.

use serde::{Deserialize, Serialize};

/// The coarse kind of a file, derived from its content type by
/// [`classify`] and stored on the blob; it picks the thumbnail icon when
/// nothing better can be rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Any `image/*` type.
    Image,
    /// Word processing and page formats: PDF, Word, ODT, RTF, EPUB.
    Document,
    /// Excel, ODS, CSV and TSV.
    Spreadsheet,
    /// PowerPoint and ODP.
    Presentation,
    /// ZIP, gzip, tar, 7z, RAR, bzip2 and zstd.
    Archive,
    /// Any `audio/*` type.
    Audio,
    /// Any `video/*` type.
    Video,
    /// Any `text/*` type except CSV and TSV, plus JSON, XML and GeoJSON.
    Text,
    /// Everything else, `application/octet-stream` included.
    Other,
}

crate::str_enum!(Category {
    Image => "image",
    Document => "document",
    Spreadsheet => "spreadsheet",
    Presentation => "presentation",
    Archive => "archive",
    Audio => "audio",
    Video => "video",
    Text => "text",
    Other => "other",
});

/// The content type of a PDF file.
pub const PDF: &str = "application/pdf";
/// The content type of a CSV file, a [`Category::Spreadsheet`].
pub const CSV: &str = "text/csv";
/// The content type of an Excel workbook (Office Open XML).
pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
/// The content type of an OpenDocument spreadsheet.
pub const ODS: &str = "application/vnd.oasis.opendocument.spreadsheet";
/// The content type of a ZIP archive, also the container of XLSX, DOCX,
/// ODS and ODT.
pub const ZIP: &str = "application/zip";
/// The content type of a Word document (Office Open XML).
pub const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
/// The content type of an OpenDocument text.
pub const ODT: &str = "application/vnd.oasis.opendocument.text";

/// Classify a normalised `type/subtype` content type.
pub fn classify(content_type: &str) -> Category {
    let exact = match content_type {
        PDF | "application/msword" | DOCX | ODT | "application/rtf" | "application/epub+zip" => {
            Some(Category::Document)
        }

        "application/vnd.ms-excel" | XLSX | ODS | CSV | "text/tab-separated-values" => {
            Some(Category::Spreadsheet)
        }

        "application/vnd.ms-powerpoint"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.oasis.opendocument.presentation" => Some(Category::Presentation),

        ZIP
        | "application/x-zip-compressed"
        | "application/gzip"
        | "application/x-tar"
        | "application/x-7z-compressed"
        | "application/x-rar-compressed"
        | "application/vnd.rar"
        | "application/x-bzip2"
        | "application/zstd" => Some(Category::Archive),

        "application/json" | "application/xml" | crate::geojson::GEOJSON => Some(Category::Text),

        _ => None,
    };
    if let Some(category) = exact {
        return category;
    }
    match content_type.split_once('/').map(|(t, _)| t) {
        Some("image") => Category::Image,
        Some("audio") => Category::Audio,
        Some("video") => Category::Video,
        Some("text") => Category::Text,
        _ => Category::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies() {
        assert_eq!(classify("image/jpeg"), Category::Image);
        assert_eq!(classify("image/heic"), Category::Image);
        assert_eq!(classify(PDF), Category::Document);
        assert_eq!(classify(XLSX), Category::Spreadsheet);
        assert_eq!(classify(CSV), Category::Spreadsheet);
        assert_eq!(classify(ZIP), Category::Archive);
        assert_eq!(classify("text/plain"), Category::Text);
        assert_eq!(classify("application/json"), Category::Text);
        assert_eq!(classify(crate::geojson::GEOJSON), Category::Text);
        assert_eq!(classify("video/mp4"), Category::Video);
        assert_eq!(classify("application/octet-stream"), Category::Other);
    }
}
