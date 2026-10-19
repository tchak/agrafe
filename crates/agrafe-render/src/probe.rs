//! A check of the tools on known-good files, run before a worker starts.

use std::path::Path;

use crate::helpers;
use crate::vips::{self, Fit, Input};
use crate::{Config, Error, Rejection, Verdict};

/// Why a worker may not start: its tools do not work on files known to be
/// good.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("rendering {what} with `{vips}`: {source}")]
    Render {
        what: &'static str,
        vips: String,
        source: Error,
    },
    #[error("rendering {what} with `{vips}`: {rejection}")]
    Rejected {
        what: &'static str,
        vips: String,
        rejection: Rejection,
    },
    #[error("`{command}` {what}; is media.render_text_command the agrafe-render-text helper?")]
    Helper { command: String, what: String },
    #[error(transparent)]
    Run(#[from] Error),
    #[error("temporary files: {0}")]
    Io(#[from] std::io::Error),
}

const PROBE_PNG: &[u8] = include_bytes!("../assets/probe/pixel.png");
const PROBE_PDF: &[u8] = include_bytes!("../assets/probe/page.pdf");

/// Render a text sample, a one-pixel PNG and a one-page PDF through the
/// exact command lines the jobs use. vips exits non-zero for a bad file and
/// for a missing loader or an unknown option alike, so without this a
/// libvips built without PDF support, or too old for an option, would mark
/// every file `failed` instead of failing the deployment. The text sample
/// also catches a `render_text_command` that is not the helper.
pub async fn probe(config: &Config, tmp_dir: Option<&Path>) -> Result<(), ProbeError> {
    let workdir = crate::workdir(tmp_dir)?;
    let fit = Fit::thumbnail(config);
    let sample = workdir.path().join("sample.txt");
    tokio::fs::write(&sample, b"agrafe\n").await?;
    let page = workdir.path().join("page.pgm");
    let what = match helpers::render_text(config, &sample, &page, fit).await? {
        Verdict::Rendered(()) => None,
        Verdict::Declined => Some("declined a text sample".to_string()),
        Verdict::Rejected(rejection) => Some(format!("failed on a text sample: {rejection}")),
    };
    if let Some(what) = what {
        return Err(ProbeError::Helper {
            command: config.render_text_command.clone(),
            what,
        });
    }
    let pixel = workdir.path().join("pixel.png");
    tokio::fs::write(&pixel, PROBE_PNG).await?;
    let pdf = workdir.path().join("page.pdf");
    tokio::fs::write(&pdf, PROBE_PDF).await?;
    let output = workdir.path().join("probe.webp");
    for (what, input, kind) in [
        ("a PNG", &pixel, Input::Raster),
        ("a PDF page", &pdf, Input::Page),
        ("a text page", &page, Input::Raster),
    ] {
        let vips = config.vips_command.clone();
        let verdict = vips::to_webp(config, input, kind, &output, fit)
            .await
            .map_err(|source| ProbeError::Render {
                what,
                vips: vips.clone(),
                source,
            })?;
        if let Verdict::Rejected(rejection) = verdict {
            return Err(ProbeError::Rejected {
                what,
                vips,
                rejection,
            });
        }
    }
    Ok(())
}
