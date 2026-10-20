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
pub mod probe;
pub mod vips;

use std::path::Path;

use agrafe_types::classify::{self, Category};
use agrafe_types::geojson::GEOJSON;
use agrafe_types::sniff;
use serde::Deserialize;

pub use agrafe_types::classify::{DOCX, ODT};
pub use geo::GeoJson;
pub use probe::{ProbeError, probe, probe_document, probe_sandbox};

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
    /// The helper that draws a GeoJSON file as SVG, found on `PATH` by
    /// default.
    #[serde(default = "default_render_geo_command")]
    pub render_geo_command: String,
    #[serde(default)]
    pub thumbnail: Thumbnail,
    #[serde(default)]
    pub preview: Preview,
    #[serde(default)]
    pub embedded_image: EmbeddedImage,
    /// How the map of a GeoJSON file is drawn, and what it is drawn over.
    #[serde(default)]
    pub geojson: GeoJson,
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
            geojson: GeoJson::default(),
            sandbox: agrafe_sandbox::Config::default(),
        }
    }
}

/// The thumbnail box and encoder quality.
#[derive(Debug, Clone, Deserialize)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
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
    pub max_dimension: u32,
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
    #[error(transparent)]
    Sandbox(#[from] agrafe_sandbox::Error),
    #[error("reading the file to render: {0}")]
    Input(std::io::Error),
    #[error("reading the rendered file: {0}")]
    Output(std::io::Error),
    #[error("reading the extent the map helper wrote: {0}")]
    Extent(serde_json::Error),
    #[error("fetching a background layer: {0}")]
    Fetch(#[from] geo::FetchError),
}

/// How an attempt at rendering ended. Anything that is not a failure of
/// ours is a verdict on the file, and trying the same bytes again would
/// end the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict<T> {
    Rendered(T),
    /// The file's fault: the tool rejected it, crashed on it, or ran out of
    /// time.
    Rejected(Rejection),
    /// Nothing to render: no embedded image, a sample that is not text.
    Declined,
}

impl<T> Verdict<T> {
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
    #[error("the file is not in a format the renderer may load")]
    Unsupported,
    #[error("the tool failed ({status}): {stderr}")]
    Failed {
        /// The exit code, `None` for a signal.
        code: Option<i32>,
        status: String,
        stderr: String,
    },
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
    /// A GeoJSON file: its geometry drawn over a map background.
    GeoJson,
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
/// which is more attack surface than it is worth. A PDF or an office file
/// that wraps one picture is rendered from it; otherwise a PDF gets its
/// first page as a thumbnail and an office file the icon. A GeoJSON file
/// gets a map, and the text sample of the text file it is when it holds
/// no geometry. The `text` category, plus CSV and TSV, which classify as
/// spreadsheets, get a text sample as a thumbnail.
pub fn plan(content_type: &str) -> Plan {
    let canonical = sniff::canonical(content_type);
    match canonical {
        GEOJSON => Plan {
            picture: &[Source::GeoJson],
            fallback: Some(Fallback::TextSample),
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
        DOCX | ODT => Plan {
            picture: &[Source::EmbeddedImage],
            fallback: None,
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
        for office in [DOCX, ODT] {
            assert_eq!(plan(office).picture, &[Source::EmbeddedImage], "{office}");
            assert_eq!(plan(office).fallback, None, "{office}");
        }
        assert_eq!(
            plan(GEOJSON),
            Plan {
                picture: &[Source::GeoJson],
                fallback: Some(Fallback::TextSample),
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
            classify::XLSX,
            "image/vnd.adobe.photoshop",
            "application/octet-stream",
        ] {
            assert_eq!(plan(other), Plan::NONE, "{other}");
        }
    }
}
