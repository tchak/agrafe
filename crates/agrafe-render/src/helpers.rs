//! The sandboxed helpers: `agrafe-extract` pulls the embedded image out of
//! a document, `agrafe-render-text` rasterises a text sample. Each is a
//! separate binary with its own parsers, so none of that code is linked
//! into the process that calls it. A helper that succeeds without writing
//! its output found nothing to do.

use std::path::Path;

use agrafe_sandbox::{self as sandbox, Outcome};

use crate::vips::Fit;
use crate::{Config, Error};

/// How a helper run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Helper {
    /// It wrote its output.
    Produced,
    /// It found nothing to do: no embedded image, a sample that is not text.
    Declined,
    /// It crashed or ran out of time, which the file takes the blame for.
    Failed,
}

/// Run `agrafe-extract` on `original`, writing the embedded image, if there
/// is one, to `output`.
pub async fn extract(
    config: &Config,
    content_type: &str,
    original: &Path,
    output: &Path,
) -> Result<Helper, Error> {
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
    run(config, command, output, "extraction").await
}

/// Run `agrafe-render-text` on `sample`, drawing at twice the box so the
/// downscale leaves crisp glyphs, writing a page to `output` when the
/// sample is text.
pub async fn render_text(
    config: &Config,
    sample: &Path,
    output: &Path,
    fit: Fit,
) -> Result<Helper, Error> {
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
    run(config, command, output, "text").await
}

async fn run(
    config: &Config,
    command: tokio::process::Command,
    output: &Path,
    what: &str,
) -> Result<Helper, Error> {
    match sandbox::run(command, &config.sandbox).await? {
        Outcome::Success => Ok(
            if tokio::fs::try_exists(output).await.map_err(Error::Output)? {
                Helper::Produced
            } else {
                Helper::Declined
            },
        ),
        Outcome::Failed { status, stderr } => {
            tracing::warn!(status, stderr, "the {what} helper failed");
            Ok(Helper::Failed)
        }
        Outcome::TimedOut => {
            tracing::warn!("the {what} helper timed out");
            Ok(Helper::Failed)
        }
    }
}
