//! Rasterising a file to a bounded WebP with `vips thumbnail`.
//!
//! One operation covers every input: images are decoded with shrink-on-load
//! where the format allows it, PDFs are rendered at the scale that fits the
//! box, EXIF orientation is applied, colour is converted to sRGB and every
//! piece of metadata is dropped. The output is always a freshly encoded
//! WebP, so nothing from the input container reaches a client.

use std::path::Path;

use super::sandbox::{self, Outcome};
use crate::config::MediaConfig;

/// Target box and encoder quality. The image is scaled to fit inside the
/// box, never cropped.
#[derive(Debug, Clone, Copy)]
pub struct Fit {
    pub width: u32,
    pub height: u32,
    pub quality: u8,
}

impl Fit {
    pub fn thumbnail(media: &MediaConfig) -> Self {
        Self {
            width: media.thumbnail.width,
            height: media.thumbnail.height,
            quality: media.thumbnail.quality,
        }
    }

    pub fn preview(media: &MediaConfig) -> Self {
        Self {
            width: media.preview.max_dimension,
            height: media.preview.max_dimension,
            quality: media.preview.quality,
        }
    }
}

/// What the input is, which decides whether it may be enlarged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// Pixels: scaled down to the box at most, a small image stays small.
    Raster,
    /// A vector page, with a nominal size of 72 dpi: rendered at whatever
    /// scale fills the box, since enlarging it loses nothing.
    Page,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The renderer could not make sense of the file: the file's fault.
    #[error("the renderer rejected the file ({status}): {stderr}")]
    Rejected { status: String, stderr: String },
    /// The file took too long: also the file's fault.
    #[error("the renderer exceeded the time limit")]
    TimedOut,
    #[error(transparent)]
    Sandbox(#[from] sandbox::Error),
    #[error("reading the rendered file: {0}")]
    Output(std::io::Error),
}

impl Error {
    /// Whether the input is to blame, so that trying again is pointless.
    pub fn is_input_fault(&self) -> bool {
        matches!(self, Error::Rejected { .. } | Error::TimedOut)
    }
}

/// Render `input` into `output` as a WebP fitting `fit`, and return its
/// bytes. `output` must not contain `[`, which vips reads as options.
pub async fn to_webp(
    media: &MediaConfig,
    input: &Path,
    kind: Input,
    output: &Path,
    fit: Fit,
) -> Result<Vec<u8>, Error> {
    let size = match kind {
        Input::Raster => "down",
        Input::Page => "both",
    };
    let mut command = sandbox::command(&media.vips_command);
    command
        .env("VIPS_WARNING", "0")
        .arg("thumbnail")
        .arg(input)
        .arg(format!("{}[Q={},keep=none]", output.display(), fit.quality))
        .arg(fit.width.to_string())
        .args(["--height", &fit.height.to_string()])
        .args(["--size", size])
        .args(["--output-profile", "srgb"])
        .args(["--fail-on", "error"]);
    match sandbox::run(command, &media.sandbox).await? {
        Outcome::Success => tokio::fs::read(output).await.map_err(Error::Output),
        Outcome::Failed { status, stderr } => Err(Error::Rejected { status, stderr }),
        Outcome::TimedOut => Err(Error::TimedOut),
    }
}
