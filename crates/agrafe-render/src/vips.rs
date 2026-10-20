//! Rasterising a file to a bounded WebP with `vips thumbnail`, and
//! stacking the layers of a map with `vips composite`.
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
    /// The box's width, in pixels.
    pub width: u32,
    /// The box's height, in pixels.
    pub height: u32,
    /// The WebP encoder's quality, `0..=100`.
    pub quality: u8,
}

impl Fit {
    /// The `[media.thumbnail]` box and quality.
    pub fn thumbnail(config: &Config) -> Self {
        Self {
            width: config.thumbnail.width,
            height: config.thumbnail.height,
            quality: config.thumbnail.quality,
        }
    }

    /// The `[media.preview]` box, a square, and quality.
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
    thumbnail(config, input, None, kind, output, fit).await
}

/// Render page `page`, counted from zero, of `input`, a document, like
/// [`to_webp`]. A page the document does not have is a `Rejected`, which
/// is how the probe tells a one-page PDF from a longer one without a PDF
/// parser. `input` must not contain `[` either.
pub(crate) async fn page_to_webp(
    config: &Config,
    input: &Path,
    page: u32,
    output: &Path,
    fit: Fit,
) -> Result<Verdict<Vec<u8>>, Error> {
    thumbnail(config, input, Some(page), Input::Page, output, fit).await
}

async fn thumbnail(
    config: &Config,
    input: &Path,
    page: Option<u32>,
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
    // The path as it is, with the page option appended, so that a path
    // that is not UTF-8 still names the file.
    let mut source = input.as_os_str().to_os_string();
    if let Some(page) = page {
        source.push(format!("[page={page}]"));
    }
    let mut command = sandbox::Command::new(&config.vips_command);
    if let Some(dir) = output.parent() {
        command.env("TMPDIR", dir).writable(dir);
    }
    command
        .readable(input)
        .env("VIPS_WARNING", "0")
        .env("VIPS_CONCURRENCY", VIPS_CONCURRENCY)
        .arg("thumbnail")
        .arg(source)
        .arg(format!("{}[Q={},keep=none]", output.display(), fit.quality))
        .arg(fit.width.to_string())
        .args(["--height", &fit.height.to_string()])
        .args(["--size", size])
        // The name before 8.17, kept since as an alias of `--output-profile`:
        // it works from 8.15 on, which `keep=none` needs anyway.
        .args(["--export-profile", "srgb"])
        .args(["--fail-on", "error"]);
    let outcome = sandbox::run(command, &config.sandbox).await?;
    match Rejection::of(&outcome) {
        None => Ok(Verdict::Rendered(
            tokio::fs::read(output).await.map_err(Error::Output)?,
        )),
        Some(rejection) => Ok(Verdict::Rejected(rejection)),
    }
}

/// `VipsBlendMode` `over`, the `composite` argument the command line
/// takes as a number.
const BLEND_OVER: &str = "2";

/// Stack `layers`, bottom to top, and `svg` over them into `output`, all
/// of them file names in `dir`, in vips's native format for `to_webp` to
/// pick up. The output takes the size of the bottom image, so the layers
/// must already be the SVG's size, see [`fit_layer`]. The SVG is rendered
/// at 72 dpi, where its pixel `width` and `height` are its size, which is
/// what makes it line up with the layers. vips never declines: the
/// verdict is `Rendered` or `Rejected`. The names must not contain spaces
/// or brackets, which vips reads as a separator and as options.
pub(crate) async fn composite(
    config: &Config,
    dir: &Path,
    layers: &[String],
    svg: &str,
    output: &str,
) -> Result<Verdict<()>, Error> {
    let inputs: Vec<String> = layers
        .iter()
        .map(|layer| format!("{layer}[fail-on=error]"))
        .chain(std::iter::once(format!("{svg}[dpi=72,fail-on=error]")))
        .collect();
    debug_assert!(
        inputs
            .iter()
            .all(|input| !input.contains(' ') && input.matches('[').count() == 1)
    );
    let mut command = sandbox::Command::new(&config.vips_command);
    command
        .current_dir(dir)
        .writable(dir)
        .env("TMPDIR", dir)
        .env("VIPS_WARNING", "0")
        .env("VIPS_CONCURRENCY", VIPS_CONCURRENCY)
        .arg("composite")
        .arg(inputs.join(" "))
        .arg(output)
        .arg(BLEND_OVER);
    let outcome = sandbox::run(command, &config.sandbox).await?;
    Ok(match Rejection::of(&outcome) {
        None => Verdict::Rendered(()),
        Some(rejection) => Verdict::Rejected(rejection),
    })
}

/// Decode `layer`, a background image in `dir`, into `output` in vips's
/// native format at exactly `width` by `height`, whatever size it came
/// in: a WMS may cap or round the size it was asked for, and the image
/// still covers the bounding box, so it is stretched to the box rather
/// than refused. An image already that size is copied. This is also
/// where a layer that does not decode is found out, one layer at a time,
/// before the composite. `Rejected` is the layer's fault.
pub(crate) async fn fit_layer(
    config: &Config,
    dir: &Path,
    layer: &str,
    output: &str,
    width: u32,
    height: u32,
) -> Result<Verdict<()>, Error> {
    debug_assert!(!layer.contains(['[', ' ']) && !output.contains(['[', ' ']));
    let mut command = sandbox::Command::new(&config.vips_command);
    command
        .current_dir(dir)
        .writable(dir)
        .env("TMPDIR", dir)
        .env("VIPS_WARNING", "0")
        .env("VIPS_CONCURRENCY", VIPS_CONCURRENCY)
        .arg("thumbnail")
        .arg(format!("{layer}[fail-on=error]"))
        .arg(output)
        .arg(width.to_string())
        .args(["--height", &height.to_string()])
        .args(["--size", "force"])
        .args(["--fail-on", "error"]);
    let outcome = sandbox::run(command, &config.sandbox).await?;
    Ok(match Rejection::of(&outcome) {
        None => Verdict::Rendered(()),
        Some(rejection) => Verdict::Rejected(rejection),
    })
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
