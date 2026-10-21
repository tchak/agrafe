//! Derived representations: which files can be rendered, from what, and the
//! tools that do it. Decoding and rendering never happen in the calling
//! process: `vips` and the `agrafe-extract`, `agrafe-render-text`,
//! `agrafe-render-document` and `agrafe-render-geo` helpers run as
//! subprocesses through `agrafe_sandbox`, and this crate links none of
//! their parsers. The one thing it does itself is fetch the background
//! layers of a map (`geo`), which is trusted code talking to a configured
//! provider.

pub mod geo;
pub mod helpers;
pub mod icons;
pub mod office;
pub mod probe;
pub mod vips;

use std::path::Path;

use agrafe_types::classify::{self, Category};
use agrafe_types::geojson::GEOJSON;
use agrafe_types::gpx::GPX;
use agrafe_types::kml::{KML, KMZ};
use agrafe_types::map::Format;
use agrafe_types::office::{self as formats, Application, Package};
use agrafe_types::sniff;
use serde::Deserialize;

use geo::Map;
pub use office::Office;
pub use probe::{
    ProbeError, check_office_host, probe, probe_document, probe_office, probe_sandbox,
};

/// The tools and boxes the renderer works with. Flattened into the
/// server's `[media]` section.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// The libvips command line tool, run as a sandboxed subprocess.
    #[serde(default = "default_vips_command")]
    pub vips_command: String,
    /// The helper that pulls embedded images out of documents, found on
    /// `PATH` by default.
    #[serde(default = "default_extract_command")]
    pub extract_command: String,
    /// The helper that rasterises text samples, found on `PATH` by default.
    #[serde(default = "default_render_text_command")]
    pub render_text_command: String,
    /// The helper that compiles a typst workspace into a PDF, found on
    /// `PATH` by default.
    #[serde(default = "default_render_document_command")]
    pub render_document_command: String,
    /// The helper that draws a GeoJSON, GPX, KML or KMZ file as SVG,
    /// found on `PATH` by default.
    #[serde(default = "default_render_geo_command")]
    pub render_geo_command: String,
    /// The thumbnail box and quality, the `[media.thumbnail]` table.
    #[serde(default)]
    pub thumbnail: Thumbnail,
    /// The preview box and quality, the `[media.preview]` table.
    #[serde(default)]
    pub preview: Preview,
    /// When a document counts as a wrapped image, the
    /// `[media.embedded_image]` table.
    #[serde(default)]
    pub embedded_image: EmbeddedImage,
    /// How the map of a GeoJSON, GPX, KML or KMZ file is drawn, and what
    /// it is drawn over, the `[media.map]` table.
    #[serde(default)]
    pub map: Map,
    /// The optional LibreOffice that converts office files for their
    /// first-page thumbnail, the `[media.office]` table.
    #[serde(default)]
    pub office: Office,
    /// The limits on each tool's process and whether the kernel confines
    /// it, the `[media.sandbox]` table.
    #[serde(default)]
    pub sandbox: agrafe_sandbox::Config,
}

fn default_vips_command() -> String {
    "vips".to_string()
}
fn default_extract_command() -> String {
    "agrafe-extract".to_string()
}
fn default_render_text_command() -> String {
    "agrafe-render-text".to_string()
}
fn default_render_document_command() -> String {
    "agrafe-render-document".to_string()
}
fn default_render_geo_command() -> String {
    "agrafe-render-geo".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            vips_command: default_vips_command(),
            extract_command: default_extract_command(),
            render_text_command: default_render_text_command(),
            render_document_command: default_render_document_command(),
            render_geo_command: default_render_geo_command(),
            thumbnail: Thumbnail::default(),
            preview: Preview::default(),
            embedded_image: EmbeddedImage::default(),
            map: Map::default(),
            office: Office::default(),
            sandbox: agrafe_sandbox::Config::default(),
        }
    }
}

/// The thumbnail box and encoder quality.
#[derive(Debug, Clone, Deserialize)]
pub struct Thumbnail {
    /// The box's width in pixels, 256 by default.
    pub width: u32,
    /// The box's height in pixels, 256 by default.
    pub height: u32,
    /// The WebP quality, 80 by default.
    pub quality: u8,
}

impl Default for Thumbnail {
    fn default() -> Self {
        Self {
            width: 256,
            height: 256,
            quality: 80,
        }
    }
}

/// The preview box (square) and encoder quality.
#[derive(Debug, Clone, Deserialize)]
pub struct Preview {
    /// The side of the box in pixels, 2048 by default.
    pub max_dimension: u32,
    /// The WebP quality, 85 by default.
    pub quality: u8,
}

impl Default for Preview {
    fn default() -> Self {
        Self {
            max_dimension: 2048,
            quality: 85,
        }
    }
}

/// What makes a document "really an image", passed to `agrafe-extract`.
#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddedImage {
    /// Fraction of the page the image must cover (PDF only).
    pub min_coverage: f32,
    /// Non-whitespace characters of text allowed around it.
    pub max_text_chars: u32,
}

impl Default for EmbeddedImage {
    fn default() -> Self {
        Self {
            min_coverage: 0.7,
            max_text_chars: 50,
        }
    }
}

/// A failure of ours, which the caller may retry: the sandbox, the work
/// directory, the provider of a map's background. The file's own faults
/// are a [`Verdict::Rejected`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A tool could not be started, waited for or confined.
    #[error(transparent)]
    Sandbox(#[from] agrafe_sandbox::Error),
    /// The input could not be read, or the helper's spec written.
    #[error("reading the file to render: {0}")]
    Input(std::io::Error),
    /// What the tool wrote could not be read back.
    #[error("reading the rendered file: {0}")]
    Output(std::io::Error),
    /// A background layer of a map could not be fetched or decoded.
    #[error("fetching a background layer: {0}")]
    Fetch(#[from] geo::FetchError),
}

/// How an attempt at rendering ended. Anything that is not a failure of
/// ours is a verdict on the file, and trying the same bytes again would
/// end the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict<T> {
    /// The tool succeeded, with what it produced.
    Rendered(T),
    /// The file's fault: the tool rejected it, crashed on it, or ran out of
    /// time.
    Rejected(Rejection),
    /// Nothing to render: no embedded image, a sample that is not text.
    Declined,
}

impl<T> Verdict<T> {
    /// The verdict with `f` applied to what was rendered, if anything was.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Verdict<U> {
        match self {
            Verdict::Rendered(value) => Verdict::Rendered(f(value)),
            Verdict::Rejected(rejection) => Verdict::Rejected(rejection),
            Verdict::Declined => Verdict::Declined,
        }
    }

    /// What was rendered, if anything was.
    pub fn rendered(self) -> Option<T> {
        match self {
            Verdict::Rendered(value) => Some(value),
            Verdict::Rejected(_) | Verdict::Declined => None,
        }
    }
}

/// Why a tool would not render a file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Rejection {
    /// The file's magic bytes match no loader on the allowlist, so `vips`
    /// is never run on it.
    #[error("the file is not in a format the renderer may load")]
    Unsupported,
    /// The tool exited non-zero or was killed by a signal.
    #[error("the tool failed ({status}): {stderr}")]
    Failed {
        /// The exit code, `None` for a signal.
        code: Option<i32>,
        /// The exit status, as the system describes it.
        status: String,
        /// The end of what the tool wrote on stderr.
        stderr: String,
    },
    /// The tool was killed at `media.sandbox.timeout`.
    #[error("the tool exceeded the time limit")]
    TimedOut,
    /// A helper ran to completion and wrote something other than what it
    /// was asked for: it is the untrusted side, and what drove it off
    /// course is the file.
    #[error("the helper's output is not what it was asked for: {0}")]
    Output(String),
}

impl Rejection {
    /// The verdict on a sandboxed tool that ran to completion; `None` when
    /// it succeeded.
    pub(crate) fn of(outcome: &agrafe_sandbox::Outcome) -> Option<Self> {
        match outcome {
            agrafe_sandbox::Outcome::Success => None,
            agrafe_sandbox::Outcome::Failed {
                code,
                status,
                stderr,
            } => Some(Rejection::Failed {
                code: *code,
                status: status.clone(),
                stderr: stderr.clone(),
            }),
            agrafe_sandbox::Outcome::TimedOut => Some(Rejection::TimedOut),
        }
    }
}

/// A fresh directory for one render's files, under `tmp_dir` or the system
/// temporary directory. Removed when dropped.
pub fn workdir(tmp_dir: Option<&Path>) -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("agrafe-")
        .tempdir_in(tmp_dir.map_or_else(std::env::temp_dir, Path::to_path_buf))
}

/// Where the picture of a file comes from: the file rendered as a whole,
/// which is the preview, and the thumbnail fitted from it. A file lists
/// its sources in the order they are tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The file is an image: decode and transcode it.
    Image,
    /// A document that wraps one image with next to no text: pull the image
    /// out at full resolution.
    EmbeddedImage,
    /// A map file in this format: its geometry drawn over a map
    /// background.
    Map(Format),
}

/// Where a thumbnail comes from when the file has no picture: a hint of
/// what the file is, not a way to read it, which is why neither is a
/// preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// The first lines of the file set in a monospace font.
    TextSample,
    /// The first page rendered.
    FirstPage,
    /// The first page, slide or drawing page rendered after LibreOffice
    /// converted the file to a PDF, when a converter is configured
    /// (`media.office.command`); the icon otherwise.
    ConvertedFirstPage,
}

/// What renders for a content type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// The sources of the file's picture, in order. Empty when the file
    /// cannot be rendered as a whole: then there is no preview, and the
    /// thumbnail comes from the fallback or is an icon.
    pub picture: &'static [Source],
    /// What the thumbnail falls back to when no source yields a picture.
    pub fallback: Option<Fallback>,
}

impl Plan {
    const NONE: Plan = Plan {
        picture: &[],
        fallback: None,
    };
}

/// The plan for a content type. Image types are an allowlist of what
/// libvips loads natively: everything else would go through ImageMagick,
/// which is more attack surface than it is worth. A PDF or a Writer
/// package (DOCX, ODT and their variants) that wraps one picture is
/// rendered from it; otherwise a PDF gets its first page as a thumbnail,
/// and every office format (`agrafe_types::office`: Word, Excel,
/// PowerPoint and OpenDocument files, their legacy, macro-enabled and
/// template variants, RTF and drawings) its first page through
/// LibreOffice when one is configured, else the icon. A GeoJSON, GPX or
/// KML file gets a map, and the text sample of the text file it is when
/// it holds no geometry; a KMZ gets a map too, and the archive icon
/// otherwise. The `text` category, plus CSV and TSV, which classify as
/// spreadsheets, get a text sample as a thumbnail.
pub fn plan(content_type: &str) -> Plan {
    let canonical = sniff::canonical(content_type);
    if let Some(format) = formats::format(canonical) {
        // The extractor opens the ZIP of a Writer package for the one
        // picture it may wrap. The legacy formats and RTF have no
        // extractor, spreadsheets are never a wrapped picture, and the
        // first picture of a presentation is as likely its logo as its
        // slide: those get the page only.
        let picture: &'static [Source] = match (format.application, format.package) {
            (Application::Writer, Package::Ooxml | Package::Odf) => &[Source::EmbeddedImage],
            _ => &[],
        };
        return Plan {
            picture,
            fallback: Some(Fallback::ConvertedFirstPage),
        };
    }
    match canonical {
        GEOJSON => Plan {
            picture: &[Source::Map(Format::GeoJson)],
            fallback: Some(Fallback::TextSample),
        },
        GPX => Plan {
            picture: &[Source::Map(Format::Gpx)],
            fallback: Some(Fallback::TextSample),
        },
        KML => Plan {
            picture: &[Source::Map(Format::Kml)],
            fallback: Some(Fallback::TextSample),
        },
        KMZ => Plan {
            picture: &[Source::Map(Format::Kmz)],
            fallback: None,
        },
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "image/tiff" | "image/heif"
        | "image/avif" | "image/svg+xml" | "image/bmp" | "image/jp2" => Plan {
            picture: &[Source::Image],
            fallback: None,
        },
        classify::PDF => Plan {
            picture: &[Source::EmbeddedImage],
            fallback: Some(Fallback::FirstPage),
        },
        classify::CSV | "text/tab-separated-values" => Plan {
            picture: &[],
            fallback: Some(Fallback::TextSample),
        },
        _ if classify::classify(canonical) == Category::Text => Plan {
            picture: &[],
            fallback: Some(Fallback::TextSample),
        },
        _ => Plan::NONE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agrafe_types::office::{
        DOC, DOCM, DOCX, DOTM, DOTX, ODG, ODM, ODP, ODS, ODT, OTG, OTH, OTM, OTP, OTS, OTT, POTM,
        POTX, PPSM, PPSX, PPT, PPTM, PPTX, RTF, XLS, XLSB, XLSM, XLSX, XLTM, XLTX,
    };

    #[test]
    fn plans_by_type() {
        for image in ["image/jpg", "image/heic", "image/svg+xml"] {
            assert_eq!(plan(image).picture, &[Source::Image], "{image}");
            assert_eq!(plan(image).fallback, None, "{image}");
        }
        assert_eq!(
            plan(classify::PDF),
            Plan {
                picture: &[Source::EmbeddedImage],
                fallback: Some(Fallback::FirstPage),
            }
        );
        // A Writer package may wrap a picture; the binary formats, RTF,
        // spreadsheets, presentations and drawings get the page only.
        for office in [DOCX, DOCM, DOTX, DOTM, ODT, OTT, ODM, OTM, OTH] {
            assert_eq!(plan(office).picture, &[Source::EmbeddedImage], "{office}");
            assert_eq!(
                plan(office).fallback,
                Some(Fallback::ConvertedFirstPage),
                "{office}"
            );
        }
        for page_only in [
            DOC, RTF, "text/rtf", XLSX, XLSM, XLTX, XLTM, XLSB, XLS, ODS, OTS, PPTX, PPTM, PPSX,
            PPSM, POTX, POTM, PPT, ODP, OTP, ODG, OTG,
        ] {
            assert!(plan(page_only).picture.is_empty(), "{page_only}");
            assert_eq!(
                plan(page_only).fallback,
                Some(Fallback::ConvertedFirstPage),
                "{page_only}"
            );
        }
        assert_eq!(
            plan(GEOJSON),
            Plan {
                picture: &[Source::Map(Format::GeoJson)],
                fallback: Some(Fallback::TextSample),
            }
        );
        assert_eq!(
            plan(GPX),
            Plan {
                picture: &[Source::Map(Format::Gpx)],
                fallback: Some(Fallback::TextSample),
            }
        );
        assert_eq!(
            plan(KML),
            Plan {
                picture: &[Source::Map(Format::Kml)],
                fallback: Some(Fallback::TextSample),
            }
        );
        assert_eq!(
            plan(KMZ),
            Plan {
                picture: &[Source::Map(Format::Kmz)],
                fallback: None,
            }
        );
        for text in [
            "text/plain",
            "application/json",
            classify::CSV,
            "text/tab-separated-values",
        ] {
            assert!(plan(text).picture.is_empty(), "{text}");
            assert_eq!(plan(text).fallback, Some(Fallback::TextSample), "{text}");
        }
        for other in [
            classify::ZIP,
            "image/vnd.adobe.photoshop",
            "application/octet-stream",
            "application/x-ole-storage",
            "application/vnd.ms-excel.addin.macroenabled.12",
        ] {
            assert_eq!(plan(other), Plan::NONE, "{other}");
        }
    }
}
