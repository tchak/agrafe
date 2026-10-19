//! Rasterising a file to a bounded WebP with `vips thumbnail`.
//!
//! One operation covers every input: images are decoded with shrink-on-load
//! where the format allows it, PDFs are rendered at the scale that fits the
//! box, EXIF orientation is applied, colour is converted to sRGB and every
//! piece of metadata is dropped. The output is always a freshly encoded
//! WebP, so nothing from the input container reaches a client.

use std::path::Path;

use agrafe_sandbox as sandbox;
use agrafe_types::sniff::{LOADER_SNIFF_LEN, loader_for};
use tokio::io::AsyncReadExt;

use crate::{Config, Error, Rejection, Verdict};

/// Threads per vips process. Parallelism comes from running several
/// renders at once (`jobs.concurrency`), so each one is kept narrow: the
/// address space limit is then the same on a 4-core box and a 64-core one.
const VIPS_CONCURRENCY: &str = "2";

/// Target box and encoder quality. The image is scaled to fit inside the
/// box, never cropped.
#[derive(Debug, Clone, Copy)]
pub struct Fit {
    pub width: u32,
    pub height: u32,
    pub quality: u8,
}

impl Fit {
    pub fn thumbnail(config: &Config) -> Self {
        Self {
            width: config.thumbnail.width,
            height: config.thumbnail.height,
            quality: config.thumbnail.quality,
        }
    }

    pub fn preview(config: &Config) -> Self {
        Self {
            width: config.preview.max_dimension,
            height: config.preview.max_dimension,
            quality: config.preview.quality,
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

impl Input {
    /// What an image file is. SVG is a page: it has a nominal size, and
    /// filling the box loses nothing.
    pub fn for_image(content_type: &str) -> Self {
        if content_type == "image/svg+xml" {
            Input::Page
        } else {
            Input::Raster
        }
    }
}

/// Render `input` into `output` as a WebP fitting `fit`, and return its
/// bytes. `output` must not contain `[`, which vips reads as options, and
/// its directory is where vips spills intermediates. vips never declines:
/// the verdict is `Rendered` or `Rejected`.
pub async fn to_webp(
    config: &Config,
    input: &Path,
    kind: Input,
    output: &Path,
    fit: Fit,
) -> Result<Verdict<Vec<u8>>, Error> {
    let Some(loader) = loader_for(&head(input).await.map_err(Error::Input)?) else {
        return Ok(Verdict::Rejected(Rejection::Unsupported));
    };
    tracing::debug!(loader, "rendering");
    let size = match kind {
        Input::Raster => "down",
        Input::Page => "both",
    };
    let mut command = sandbox::command(&config.vips_command);
    if let Some(dir) = output.parent() {
        command.env("TMPDIR", dir);
    }
    command
        .env("VIPS_WARNING", "0")
        .env("VIPS_CONCURRENCY", VIPS_CONCURRENCY)
        .arg("thumbnail")
        .arg(input)
        .arg(format!("{}[Q={},keep=none]", output.display(), fit.quality))
        .arg(fit.width.to_string())
        .args(["--height", &fit.height.to_string()])
        .args(["--size", size])
        .args(["--output-profile", "srgb"])
        .args(["--fail-on", "error"]);
    let outcome = sandbox::run(command, &config.sandbox).await?;
    match Rejection::of(&outcome) {
        None => Ok(Verdict::Rendered(
            tokio::fs::read(output).await.map_err(Error::Output)?,
        )),
        Some(rejection) => Ok(Verdict::Rejected(rejection)),
    }
}

/// The first `LOADER_SNIFF_LEN` bytes of `path`, fewer for a shorter file.
async fn head(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(LOADER_SNIFF_LEN);
    tokio::fs::File::open(path)
        .await?
        .take(LOADER_SNIFF_LEN as u64)
        .read_to_end(&mut head)
        .await?;
    Ok(head)
}
