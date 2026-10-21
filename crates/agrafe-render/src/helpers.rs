//! The sandboxed helpers: `agrafe-extract` pulls the embedded image out of
//! a document, `agrafe-render-text` rasterises a text sample,
//! `agrafe-render-document` compiles a typst workspace into a PDF,
//! `agrafe-render-geo` draws a map file as SVG. Each is a separate
//! binary with its own parsers, so none of that code is linked into the
//! process that calls it. Each declares what it reads and the one
//! directory it writes, which is what the sandbox confines it to. A
//! helper that succeeds without writing its output found nothing to do.

use std::path::Path;

use agrafe_sandbox as sandbox;

use crate::vips::Fit;
use crate::{Config, Error, Rejection, Verdict};

/// Run `agrafe-extract` on `original`, writing the embedded image, if there
/// is one, to `output`.
pub async fn extract(
    config: &Config,
    content_type: &str,
    original: &Path,
    output: &Path,
) -> Result<Verdict<()>, Error> {
    let mut command = sandbox::Command::new(&config.extract_command);
    command
        .readable(original)
        .arg("--input")
        .arg(original)
        .arg("--output")
        .arg(output)
        .arg("--content-type")
        .arg(content_type)
        .arg("--min-coverage")
        .arg(config.embedded_image.min_coverage.to_string())
        .arg("--max-text-chars")
        .arg(config.embedded_image.max_text_chars.to_string())
        .arg("--max-output")
        .arg(config.sandbox.max_output_size.as_u64().to_string());
    run(config, command, output).await
}

/// Run `agrafe-render-text` on `sample`, drawing at twice the box so the
/// downscale leaves crisp glyphs, writing a page to `output` when the
/// sample is text.
pub async fn render_text(
    config: &Config,
    sample: &Path,
    output: &Path,
    fit: Fit,
) -> Result<Verdict<()>, Error> {
    let mut command = sandbox::Command::new(&config.render_text_command);
    command
        .readable(sample)
        .arg("--input")
        .arg(sample)
        .arg("--output")
        .arg(output)
        .arg("--width")
        .arg((fit.width * 2).to_string())
        .arg("--height")
        .arg((fit.height * 2).to_string());
    run(config, command, output).await
}

/// The exit code `agrafe-render-document` reserves for a document that
/// does not compile, its diagnostics on stderr. Any other failure is the
/// helper's own: it could not read its workspace or write its output.
/// Mirrored in the helper crate, which the server does not link.
pub const DOCUMENT_REJECTED_EXIT: i32 = 2;

/// Run `agrafe-render-document` on the `workspace` directory, compiling
/// `main`, a path relative to it, into the PDF at `output`. A document
/// that does not compile is a `Rejected` with [`DOCUMENT_REJECTED_EXIT`]
/// whose stderr holds the diagnostics.
pub async fn render_document(
    config: &Config,
    workspace: &Path,
    main: &str,
    output: &Path,
) -> Result<Verdict<()>, Error> {
    let mut command = sandbox::Command::new(&config.render_document_command);
    command
        .readable(workspace)
        .arg("--workspace")
        .arg(workspace)
        .arg("--main")
        .arg(main)
        .arg("--output")
        .arg(output);
    run(config, command, output).await
}

/// Run `agrafe-render-geo` on `original`, a file in the format `spec`
/// names, as `spec` asks, writing the geometry as SVG paths to `svg` and
/// the extent it chose to `extent` when the file holds any geometry.
pub(crate) async fn render_geo(
    config: &Config,
    original: &Path,
    spec: &Path,
    svg: &Path,
    extent: &Path,
) -> Result<Verdict<()>, Error> {
    let mut command = sandbox::Command::new(&config.render_geo_command);
    command
        .readable(original)
        .readable(spec)
        .arg("--input")
        .arg(original)
        .arg("--spec")
        .arg(spec)
        .arg("--svg")
        .arg(svg)
        .arg("--extent")
        .arg(extent);
    run(config, command, svg).await
}

/// Run the helper with `output`'s directory as the one it may write.
/// `Rendered` when the helper wrote `output`, `Declined` when it exited
/// cleanly without, `Rejected` when it crashed or ran out of time.
async fn run(
    config: &Config,
    mut command: sandbox::Command,
    output: &Path,
) -> Result<Verdict<()>, Error> {
    if let Some(dir) = output.parent() {
        command.writable(dir);
    }
    let outcome = sandbox::run(command, &config.sandbox).await?;
    if let Some(rejection) = Rejection::of(&outcome) {
        return Ok(Verdict::Rejected(rejection));
    }
    Ok(
        if tokio::fs::try_exists(output).await.map_err(Error::Output)? {
            Verdict::Rendered(())
        } else {
            Verdict::Declined
        },
    )
}
