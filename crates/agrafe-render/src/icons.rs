//! Pre-rendered icons served in place of a thumbnail: one per category,
//! with a type badge for the common formats, in each output format a
//! deployment can choose (`media.format`). Rendered from
//! `assets/icons/icon.svg` by `scripts/icons.sh` and embedded in the
//! binary, so serving one needs no renderer and no asset directory.

use agrafe_types::ImageFormat;
use agrafe_types::classify::{self, Category, DOCX, ODT};

/// Edge of the square icons, in pixels.
pub const SIZE: u32 = 256;

/// One icon in every format, picked by [`ImageFormat`].
macro_rules! icon {
    ($format:expr, $name:literal) => {
        match $format {
            ImageFormat::Webp => include_bytes!(concat!("../assets/icons/", $name, ".webp")),
            ImageFormat::Jxl => include_bytes!(concat!("../assets/icons/", $name, ".jxl")),
        }
    };
}

/// The icon for a file, in the given format: its type's badge when it has
/// one, else its category's.
pub fn icon_for(format: ImageFormat, category: Category, content_type: &str) -> &'static [u8] {
    match content_type {
        classify::PDF => icon!(format, "pdf"),
        DOCX | "application/msword" => icon!(format, "docx"),
        ODT => icon!(format, "odt"),
        classify::XLSX | "application/vnd.ms-excel" => icon!(format, "xlsx"),
        classify::ODS => icon!(format, "ods"),
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.ms-powerpoint" => icon!(format, "pptx"),
        "application/vnd.oasis.opendocument.presentation" => icon!(format, "odp"),
        classify::CSV => icon!(format, "csv"),
        classify::ZIP | "application/x-zip-compressed" => icon!(format, "zip"),
        "application/json" | "application/geo+json" => icon!(format, "json"),
        "application/xml" | "text/xml" => icon!(format, "xml"),
        "image/svg+xml" => icon!(format, "svg"),
        "audio/mpeg" => icon!(format, "mp3"),
        "video/mp4" => icon!(format, "mp4"),
        _ => match category {
            Category::Image => icon!(format, "image"),
            Category::Document => icon!(format, "document"),
            Category::Spreadsheet => icon!(format, "spreadsheet"),
            Category::Presentation => icon!(format, "presentation"),
            Category::Archive => icon!(format, "archive"),
            Category::Audio => icon!(format, "audio"),
            Category::Video => icon!(format, "video"),
            Category::Text => icon!(format, "text"),
            Category::Other => icon!(format, "other"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILES: [(Category, &str); 23] = [
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
    ];

    /// Every icon is in the format asked for, and the set is complete and
    /// distinct in each.
    #[test]
    fn every_icon_is_in_its_format() {
        for format in ImageFormat::ALL {
            let mut seen = std::collections::HashSet::new();
            for (category, content_type) in FILES {
                let icon = icon_for(*format, category, content_type);
                match format {
                    ImageFormat::Webp => {
                        assert_eq!(&icon[..4], b"RIFF", "{content_type}");
                        assert_eq!(&icon[8..12], b"WEBP", "{content_type}");
                    }
                    // A bare codestream; the script strips the metadata
                    // that would call for the container.
                    ImageFormat::Jxl => assert_eq!(&icon[..2], b"\xff\x0a", "{content_type}"),
                }
                assert_eq!(
                    agrafe_types::sniff::loader_for(icon),
                    Some(format.as_str()),
                    "{content_type}"
                );
                seen.insert(icon.as_ptr());
            }
            assert_eq!(seen.len(), 23, "every {format} icon is distinct");
        }
    }
}
