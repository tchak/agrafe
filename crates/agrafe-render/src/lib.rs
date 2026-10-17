//! Derived representations: which files can be rendered, from what, and the
//! tools that do it. Decoding and rendering never happen in the calling
//! process: `vips` and the `agrafe-extract` and `agrafe-render-text`
//! helpers run as subprocesses through `agrafe_sandbox`, and this crate
//! links none of their parsers.

pub mod helpers;
pub mod icons;
pub mod probe;
pub mod vips;

use std::path::Path;

use agrafe_sandbox::Limits;
use agrafe_types::classify::{self, Category};
use agrafe_types::sniff;
use serde::Deserialize;

pub use agrafe_types::classify::{DOCX, ODT};
pub use probe::{ProbeError, probe};

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
    #[serde(default)]
    pub thumbnail: Thumbnail,
    #[serde(default)]
    pub preview: Preview,
    #[serde(default)]
    pub embedded_image: EmbeddedImage,
    #[serde(default)]
    pub sandbox: Limits,
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

impl Default for Config {
    fn default() -> Self {
        Self {
            vips_command: default_vips_command(),
            extract_command: default_extract_command(),
            render_text_command: default_render_text_command(),
            thumbnail: Thumbnail::default(),
            preview: Preview::default(),
            embedded_image: EmbeddedImage::default(),
            sandbox: Limits::default(),
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

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The bytes are not a format the renderer may load: the file's fault.
    #[error("the file is not in a format the renderer may load")]
    Unsupported,
    /// The renderer could not make sense of the file: the file's fault.
    #[error("the renderer rejected the file ({status}): {stderr}")]
    Rejected { status: String, stderr: String },
    /// The file took too long: also the file's fault.
    #[error("the renderer exceeded the time limit")]
    TimedOut,
    #[error(transparent)]
    Sandbox(#[from] agrafe_sandbox::Error),
    #[error("reading the file to render: {0}")]
    Input(std::io::Error),
    #[error("reading the rendered file: {0}")]
    Output(std::io::Error),
}

impl Error {
    /// Whether the input is to blame, so that trying again is pointless.
    pub fn is_input_fault(&self) -> bool {
        matches!(
            self,
            Error::Unsupported | Error::Rejected { .. } | Error::TimedOut
        )
    }
}

/// A fresh directory for one render's files, under `tmp_dir` or the system
/// temporary directory. Removed when dropped.
pub fn workdir(tmp_dir: Option<&Path>) -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("agrafe-")
        .tempdir_in(tmp_dir.map_or_else(std::env::temp_dir, Path::to_path_buf))
}

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

#[cfg(test)]
mod tests {
    use super::*;

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
