//! Content type to category mapping.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Image,
    Document,
    Spreadsheet,
    Presentation,
    Archive,
    Audio,
    Video,
    Text,
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

/// Well-known content types.
pub const PDF: &str = "application/pdf";
pub const CSV: &str = "text/csv";
pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
pub const ODS: &str = "application/vnd.oasis.opendocument.spreadsheet";
pub const ZIP: &str = "application/zip";

/// Classify a normalised `type/subtype` content type.
pub fn classify(content_type: &str) -> Category {
    let exact = match content_type {
        PDF
        | "application/msword"
        | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.oasis.opendocument.text"
        | "application/rtf"
        | "application/epub+zip" => Some(Category::Document),

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

        "application/json" | "application/xml" | "application/geo+json" => Some(Category::Text),

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
        assert_eq!(classify("video/mp4"), Category::Video);
        assert_eq!(classify("application/octet-stream"), Category::Other);
    }
}
