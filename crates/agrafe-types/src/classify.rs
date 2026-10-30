//! Content type to category mapping.

use serde::{Deserialize, Serialize};

use crate::office::{self, Application};

/// The coarse kind of a file, derived from its content type by
/// [`classify`] and stored on the blob; it picks the thumbnail icon when
/// nothing better can be rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Any `image/*` type.
    Image,
    /// Word processing and page formats: PDF, Word, ODT, RTF, EPUB, and
    /// the drawings of Draw (ODG), pages too.
    Document,
    /// Excel, ODS, CSV and TSV.
    Spreadsheet,
    /// PowerPoint and ODP.
    Presentation,
    /// ZIP, gzip, tar, 7z, RAR, bzip2, zstd, xz, lzip, lz4 and compress,
    /// and the ZIPs that are a KMZ, a zipped shapefile, a JAR or an APK.
    Archive,
    /// Any `audio/*` type.
    Audio,
    /// Any `video/*` type.
    Video,
    /// Any `text/*` type except CSV and TSV, plus JSON, XML, every
    /// `+json` or `+xml` type (GeoJSON, GPX, KML, XHTML, RSS, JSON-LD) and
    /// the script and configuration types (JavaScript, YAML, TOML, SQL,
    /// shell, JSON lines).
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
pub const XLSX: &str = office::XLSX;
/// The content type of an OpenDocument spreadsheet.
pub const ODS: &str = office::ODS;
/// The content type of a ZIP archive, also the container of XLSX, DOCX,
/// ODS and ODT.
pub const ZIP: &str = "application/zip";
/// The content type of a Word document (Office Open XML).
pub const DOCX: &str = office::DOCX;
/// The content type of an OpenDocument text.
pub const ODT: &str = office::ODT;

/// Classify a normalised `type/subtype` content type. An alias is
/// classified as its canonical type (`sniff::canonical`), so `text/rtf`
/// is a document like `application/rtf` and `application/x-gzip` an
/// archive like `application/gzip`. An office format classifies by the
/// application that reads it (`office::format`): every Writer and Draw
/// format is a document, every Calc one a spreadsheet, every Impress one
/// a presentation, the macro-enabled, template and legacy variants
/// included. A `+json` or `+xml` type is text, whatever the registry
/// put before the suffix.
pub fn classify(content_type: &str) -> Category {
    let content_type = crate::sniff::canonical(content_type);
    if let Some(format) = office::format(content_type) {
        return match format.application {
            Application::Writer | Application::Draw => Category::Document,
            Application::Calc => Category::Spreadsheet,
            Application::Impress => Category::Presentation,
        };
    }
    let exact = match content_type {
        PDF | "application/epub+zip" => Some(Category::Document),

        CSV | "text/tab-separated-values" => Some(Category::Spreadsheet),

        ZIP
        | "application/gzip"
        | "application/x-tar"
        | "application/x-7z-compressed"
        | "application/vnd.rar"
        | "application/x-bzip2"
        | "application/zstd"
        | "application/x-xz"
        | "application/x-lzip"
        | "application/x-lz4"
        | "application/x-compress"
        | "application/vnd.android.package-archive"
        | "application/java-archive"
        | crate::kml::KMZ
        | crate::shapefile::SHAPEFILE => Some(Category::Archive),

        "application/json"
        | "application/x-ndjson"
        | "application/jsonl"
        | "application/javascript"
        | "application/x-javascript"
        | "application/ecmascript"
        | "application/yaml"
        | "application/x-yaml"
        | "application/toml"
        | "application/sql"
        | "application/x-sh"
        | "application/x-shellscript" => Some(Category::Text),

        _ => None,
    };
    if let Some(category) = exact {
        return category;
    }
    let (major, minor) = content_type.split_once('/').unwrap_or((content_type, ""));
    match major {
        "image" => Category::Image,
        "audio" => Category::Audio,
        "video" => Category::Video,
        "text" => Category::Text,
        "application" if minor.ends_with("+json") || minor.ends_with("+xml") => Category::Text,
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
        assert_eq!(classify(crate::gpx::GPX), Category::Text);
        assert_eq!(classify(crate::kml::KML), Category::Text);
        assert_eq!(classify(crate::kml::KMZ), Category::Archive);
        assert_eq!(classify(crate::shapefile::SHAPEFILE), Category::Archive);
        assert_eq!(classify("video/mp4"), Category::Video);
        assert_eq!(classify("application/octet-stream"), Category::Other);
    }

    #[test]
    fn office_formats_classify_by_application() {
        for document in [
            DOCX,
            office::DOCM,
            office::DOTX,
            office::DOTM,
            office::DOC,
            office::RTF,
            ODT,
            office::OTT,
            office::ODM,
            office::OTM,
            office::OTH,
            office::ODG,
            office::OTG,
            "application/epub+zip",
        ] {
            assert_eq!(classify(document), Category::Document, "{document}");
        }
        for sheet in [
            XLSX,
            office::XLSM,
            office::XLTX,
            office::XLTM,
            office::XLSB,
            office::XLS,
            ODS,
            office::OTS,
            "text/tab-separated-values",
        ] {
            assert_eq!(classify(sheet), Category::Spreadsheet, "{sheet}");
        }
        for slides in [
            office::PPTX,
            office::PPTM,
            office::PPSX,
            office::PPSM,
            office::POTX,
            office::POTM,
            office::PPT,
            office::ODP,
            office::OTP,
        ] {
            assert_eq!(classify(slides), Category::Presentation, "{slides}");
        }
        // Not in the table: an add-in is not a workbook anyone opens.
        assert_eq!(
            classify("application/vnd.ms-excel.addin.macroenabled.12"),
            Category::Other
        );
    }

    #[test]
    fn aliases_classify_as_their_canonical_type() {
        assert_eq!(classify("text/rtf"), Category::Document);
        assert_eq!(classify("application/x-pdf"), Category::Document);
        assert_eq!(classify("application/x-msword"), Category::Document);
        assert_eq!(classify("application/x-zip-compressed"), Category::Archive);
        assert_eq!(classify("application/x-gzip"), Category::Archive);
        assert_eq!(classify("application/x-rar-compressed"), Category::Archive);
        assert_eq!(classify("application/x-bzip"), Category::Archive);
        assert_eq!(classify("application/xml"), Category::Text);
        assert_eq!(classify("image/jpg"), Category::Image);
    }

    #[test]
    fn text_shaped_application_types_are_text() {
        for text in [
            "application/xhtml+xml",
            "application/rss+xml",
            "application/atom+xml",
            "application/ld+json",
            "application/x-ndjson",
            "application/javascript",
            "application/x-yaml",
            "application/toml",
            "application/sql",
            "application/x-sh",
        ] {
            assert_eq!(classify(text), Category::Text, "{text}");
        }
        // The suffix rule is for `application/`: an SVG is an image, a KMZ
        // an archive, and the suffix must be the subtype's end.
        assert_eq!(classify("image/svg+xml"), Category::Image);
        assert_eq!(classify("application/epub+zip"), Category::Document);
        assert_eq!(classify("application/vnd.ms-xpsdocument"), Category::Other);
        for archive in [
            "application/x-xz",
            "application/x-lzip",
            "application/x-lz4",
            "application/x-compress",
        ] {
            assert_eq!(classify(archive), Category::Archive, "{archive}");
        }
    }

    #[test]
    fn variants_classify_on_their_own_type() {
        assert_eq!(classify("image/heic"), Category::Image);
        assert_eq!(classify("video/x-matroska"), Category::Video);
        assert_eq!(classify("font/woff2"), Category::Other);
        assert_eq!(
            classify("application/vnd.android.package-archive"),
            Category::Archive
        );
        assert_eq!(classify("application/x-java-archive"), Category::Archive);
    }
}
