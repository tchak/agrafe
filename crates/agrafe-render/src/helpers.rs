//! The sandboxed helpers: `agrafe-extract` pulls the embedded image out of
//! a document, `agrafe-render-text` rasterises a text sample,
//! `agrafe-render-document` compiles a typst workspace into a PDF. Each is
//! a separate binary with its own parsers, so none of that code is linked
//! into the process that calls it. A helper that succeeds without writing
//! its output found nothing to do.

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
    let mut command = sandbox::command(&config.extract_command);
    command
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
    let mut command = sandbox::command(&config.render_text_command);
    command
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
    let mut command = sandbox::command(&config.render_document_command);
    command
        .arg("--workspace")
        .arg(workspace)
        .arg("--main")
        .arg(main)
        .arg("--output")
        .arg(output);
    run(config, command, output).await
}

/// `Rendered` when the helper wrote `output`, `Declined` when it exited
/// cleanly without, `Rejected` when it crashed or ran out of time.
async fn run(
    config: &Config,
    command: tokio::process::Command,
    output: &Path,
) -> Result<Verdict<()>, Error> {
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
