//! Pre-rendered icons served in place of a thumbnail: one per category,
//! with a type badge for the common formats. Rendered from
//! `assets/icons/icon.svg` by `scripts/icons.sh` and embedded in the
//! binary, so serving one needs no renderer and no asset directory.

use agrafe_types::classify::{self, Category, DOCX, ODT};

/// Edge of the square icons, in pixels.
pub const SIZE: u32 = 256;

macro_rules! icon {
    ($name:literal) => {
        include_bytes!(concat!("../assets/icons/", $name, ".webp"))
    };
}

/// The icon for a file: its type's badge when it has one, else its
/// category's.
pub fn icon_for(category: Category, content_type: &str) -> &'static [u8] {
    match content_type {
        classify::PDF => icon!("pdf"),
        DOCX | "application/msword" => icon!("docx"),
        ODT => icon!("odt"),
        classify::XLSX | "application/vnd.ms-excel" => icon!("xlsx"),
        classify::ODS => icon!("ods"),
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.ms-powerpoint" => icon!("pptx"),
        "application/vnd.oasis.opendocument.presentation" => icon!("odp"),
        classify::CSV => icon!("csv"),
        classify::ZIP | "application/x-zip-compressed" => icon!("zip"),
        "application/json" | "application/geo+json" => icon!("json"),
        "application/xml" | "text/xml" => icon!("xml"),
        "image/svg+xml" => icon!("svg"),
        "audio/mpeg" => icon!("mp3"),
        "video/mp4" => icon!("mp4"),
        _ => match category {
            Category::Image => icon!("image"),
            Category::Document => icon!("document"),
            Category::Spreadsheet => icon!("spreadsheet"),
            Category::Presentation => icon!("presentation"),
            Category::Archive => icon!("archive"),
            Category::Audio => icon!("audio"),
            Category::Video => icon!("video"),
            Category::Text => icon!("text"),
            Category::Other => icon!("other"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_a_webp() {
        let mut seen = std::collections::HashSet::new();
        for (category, content_type) in [
            (Category::Document, classify::PDF),
            (Category::Document, DOCX),
            (Category::Document, ODT),
            (Category::Document, "application/rtf"),
            (Category::Spreadsheet, classify::XLSX),
            (Category::Spreadsheet, classify::ODS),
            (Category::Spreadsheet, classify::CSV),
            (Category::Spreadsheet, "text/tab-separated-values"),
            (Category::Presentation, "application/vnd.ms-powerpoint"),
            (
                Category::Presentation,
                "application/vnd.oasis.opendocument.presentation",
            ),
            (Category::Presentation, "application/x-other-slides"),
            (Category::Archive, classify::ZIP),
            (Category::Archive, "application/gzip"),
            (Category::Text, "application/json"),
            (Category::Text, "application/xml"),
            (Category::Text, "text/plain"),
            (Category::Image, "image/svg+xml"),
            (Category::Image, "image/png"),
            (Category::Audio, "audio/mpeg"),
            (Category::Audio, "audio/ogg"),
            (Category::Video, "video/mp4"),
            (Category::Video, "video/webm"),
            (Category::Other, "application/octet-stream"),
        ] {
            let icon = icon_for(category, content_type);
            assert_eq!(&icon[..4], b"RIFF", "{content_type}");
            assert_eq!(&icon[8..12], b"WEBP", "{content_type}");
            seen.insert(icon.as_ptr());
        }
        assert_eq!(seen.len(), 23, "every icon is distinct");
    }
}
