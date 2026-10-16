//! Rendering a blob's thumbnail or preview: fetch and decrypt the original,
//! pick a source, rasterise it in a sandbox, store the WebP encrypted.
//!
//! A file the renderer rejects, or that crashes a helper, leaves the
//! representation `failed` and the job done: trying the same bytes again
//! would end the same way. Only failures of ours (store, database, a
//! missing renderer) fail the job. `probe` runs the same tools on known
//! files before a worker starts, so a broken deployment fails there.

use std::ops::Range;
use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;

use super::{Job, JobError, JobKind};
use crate::app::AppState;
use crate::blob::read::{Reader, Stored};
use crate::blob::{Blob, BlobState, PreviewSource, Representation, RepresentationState};
use crate::blob::{repo, representation};
use crate::config::MediaConfig;
use crate::media::render::{self, Fit, Input};
use crate::media::sandbox::{self, Outcome};
use crate::media::{self, Source, text};

pub async fn run(state: &AppState, job: &Job) -> Result<(), JobError> {
    let kind = match job.kind {
        JobKind::Thumbnail => Representation::Thumbnail,
        JobKind::Preview => Representation::Preview,
        other => return Err(JobError::Unhandled(other)),
    };
    let id = job.blob_id.ok_or(JobError::NoBlob(job.kind))?;
    let Some(blob) = repo::find(state.pool(), id).await? else {
        tracing::info!("blob is gone, nothing to render");
        return Ok(());
    };
    if blob.state != BlobState::Ready {
        tracing::info!(state = %blob.state, "blob is not ready, nothing to render");
        return Ok(());
    }
    let media = &state.config().media;
    let version = media.variant_version;

    let too_large = blob
        .size
        .is_some_and(|size| size as u64 > media.max_source_size.as_u64());
    let sources: Vec<Source> = media::sources(&blob.content_type)
        .iter()
        .copied()
        .filter(|source| source.serves(kind) && !(too_large && source.reads_whole_file()))
        .collect();
    if sources.is_empty() {
        tracing::info!(content_type = %blob.content_type, too_large, "nothing to render a {kind} from");
        repo::finish_representation(
            state.pool(),
            id,
            kind,
            RepresentationState::Unavailable,
            None,
            version,
        )
        .await?;
        return Ok(());
    }

    let workdir = workdir(media)?;
    let stored = Stored::from_blob(&blob)?;
    // The whole original is fetched once, and only by a source that needs
    // it: a text sample reads a few KiB.
    let mut original: Option<PathBuf> = None;

    let fit = match kind {
        Representation::Thumbnail => Fit::thumbnail(media),
        Representation::Preview => Fit::preview(media),
    };
    let output = workdir.path().join("rendered.webp");
    let mut rendered = None;
    let mut rejected = false;
    for source in sources {
        let (input, kind, from) = match source {
            Source::Image => (
                whole_original(state, &blob, &stored, &workdir, &mut original).await?,
                Input::for_image(&blob.content_type),
                Some(PreviewSource::Image),
            ),
            Source::FirstPage => (
                whole_original(state, &blob, &stored, &workdir, &mut original).await?,
                Input::Page,
                Some(PreviewSource::FirstPage),
            ),
            Source::EmbeddedImage => {
                let whole = whole_original(state, &blob, &stored, &workdir, &mut original).await?;
                let embedded = workdir.path().join("embedded");
                let outcome = extract(media, &blob.content_type, &whole, &embedded).await?;
                if !produced(outcome, &mut rejected) {
                    continue;
                }
                (embedded, Input::Raster, Some(PreviewSource::EmbeddedImage))
            }
            Source::TextSample => {
                let sample = workdir.path().join("sample");
                download(
                    state,
                    &blob,
                    &stored,
                    &sample,
                    0..stored.size.min(text::SAMPLE_LEN),
                )
                .await?;
                let page = workdir.path().join("page.pgm");
                if !produced(
                    render_text(media, &sample, &page, fit).await?,
                    &mut rejected,
                ) {
                    continue;
                }
                (page, Input::Raster, None)
            }
        };
        match render::to_webp(media, &input, kind, &output, fit).await {
            Ok(bytes) => {
                rendered = Some((bytes, from));
                break;
            }
            Err(err) if err.is_input_fault() => {
                tracing::warn!(source = ?source, error = %err, "rendering failed");
                rejected = true;
            }
            Err(err) => return Err(err.into()),
        }
    }

    let (rep_state, source) = match rendered {
        Some((bytes, from)) => {
            representation::store(state, &blob, kind, &bytes).await?;
            tracing::info!(source = ?from, bytes = bytes.len(), "{kind} rendered");
            (RepresentationState::Ready, from)
        }
        None if rejected => (RepresentationState::Failed, None),
        // Every source declined, as for a DOCX that is a real document.
        None => (RepresentationState::Unavailable, None),
    };
    repo::finish_representation(state.pool(), id, kind, rep_state, source, version).await?;
    Ok(())
}

/// The whole original, decrypted into the work directory on first use.
async fn whole_original(
    state: &AppState,
    blob: &Blob,
    stored: &Stored,
    workdir: &tempfile::TempDir,
    cache: &mut Option<PathBuf>,
) -> Result<PathBuf, JobError> {
    if let Some(path) = cache {
        return Ok(path.clone());
    }
    let path = workdir.path().join("original");
    download(state, blob, stored, &path, 0..stored.size).await?;
    *cache = Some(path.clone());
    Ok(path)
}

/// Decrypt the plaintext `span` of the original into `path`.
async fn download(
    state: &AppState,
    blob: &Blob,
    stored: &Stored,
    path: &Path,
    span: Range<u64>,
) -> Result<(), JobError> {
    let mut reader = Reader::open(state, blob, stored, span).await?;
    let mut file = tokio::fs::File::create(path).await?;
    while let Some(plain) = reader.next_segment().await? {
        file.write_all(&plain).await?;
    }
    file.flush().await?;
    Ok(())
}

/// A fresh directory for one render's files, under `media.tmp_dir`.
fn workdir(media: &MediaConfig) -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("agrafe-")
        .tempdir_in(media.tmp_dir.clone().unwrap_or_else(std::env::temp_dir))
}

/// How a helper run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Helper {
    /// It wrote its output.
    Produced,
    /// It found nothing to do: no embedded image, a sample that is not text.
    Declined,
    /// It crashed or ran out of time, which the file takes the blame for.
    Failed,
}

/// Whether the helper wrote its output, noting a crash against the file.
fn produced(outcome: Helper, rejected: &mut bool) -> bool {
    match outcome {
        Helper::Produced => true,
        Helper::Declined => false,
        Helper::Failed => {
            *rejected = true;
            false
        }
    }
}

/// Run the `agrafe extract` helper on `original`, writing the embedded
/// image, if there is one, to `output`.
async fn extract(
    media: &MediaConfig,
    content_type: &str,
    original: &Path,
    output: &Path,
) -> Result<Helper, JobError> {
    let mut command = helper(media)?;
    command
        .arg("extract")
        .arg("--input")
        .arg(original)
        .arg("--output")
        .arg(output)
        .arg("--content-type")
        .arg(content_type)
        .arg("--min-coverage")
        .arg(media.embedded_image.min_coverage.to_string())
        .arg("--max-text-chars")
        .arg(media.embedded_image.max_text_chars.to_string())
        .arg("--max-output")
        .arg(media.sandbox.max_output_size.as_u64().to_string());
    run_helper(media, command, output, "extraction").await
}

/// Run the `agrafe render-text` helper on `sample`, drawing at twice the
/// box so the downscale leaves crisp glyphs, writing a page to `output`
/// when the sample is text.
async fn render_text(
    media: &MediaConfig,
    sample: &Path,
    output: &Path,
    fit: Fit,
) -> Result<Helper, JobError> {
    let mut command = helper(media)?;
    command
        .arg("render-text")
        .arg("--input")
        .arg(sample)
        .arg("--output")
        .arg(output)
        .arg("--width")
        .arg((fit.width * 2).to_string())
        .arg("--height")
        .arg((fit.height * 2).to_string());
    run_helper(media, command, output, "text").await
}

/// This binary, re-executed in the sandbox.
fn helper(media: &MediaConfig) -> Result<tokio::process::Command, JobError> {
    let program = match &media.agrafe_command {
        Some(path) => path.clone(),
        None => std::env::current_exe()?,
    };
    Ok(sandbox::command(program))
}

/// A helper that succeeds without writing `output` found nothing to do.
async fn run_helper(
    media: &MediaConfig,
    command: tokio::process::Command,
    output: &Path,
    what: &str,
) -> Result<Helper, JobError> {
    match sandbox::run(command, &media.sandbox).await? {
        Outcome::Success => Ok(if tokio::fs::try_exists(output).await? {
            Helper::Produced
        } else {
            Helper::Declined
        }),
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

/// Why a worker may not start: its tools do not work on files known to be
/// good.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("rendering {what} with `{vips}`: {source}")]
    Render {
        what: &'static str,
        vips: String,
        source: render::Error,
    },
    #[error("the render-text helper {what}; is `media.agrafe_command` this binary?")]
    Helper { what: &'static str },
    #[error(transparent)]
    Run(#[from] JobError),
    #[error("temporary files: {0}")]
    Io(#[from] std::io::Error),
}

const PROBE_PNG: &[u8] = include_bytes!("../../assets/probe/pixel.png");
const PROBE_PDF: &[u8] = include_bytes!("../../assets/probe/page.pdf");

/// Render a text sample, a one-pixel PNG and a one-page PDF through the
/// exact command lines the jobs use. Run before a worker starts: vips exits
/// non-zero for a bad file and for a missing loader or an unknown option
/// alike, so without this a libvips built without PDF support, or too old
/// for an option, would mark every file `failed` instead of failing the
/// deployment.
pub async fn probe(media: &MediaConfig) -> Result<(), ProbeError> {
    let workdir = workdir(media)?;
    let fit = Fit::thumbnail(media);
    let sample = workdir.path().join("sample.txt");
    tokio::fs::write(&sample, b"agrafe\n").await?;
    let page = workdir.path().join("page.pgm");
    match render_text(media, &sample, &page, fit).await? {
        Helper::Produced => {}
        Helper::Declined => {
            return Err(ProbeError::Helper {
                what: "declined a text sample",
            });
        }
        Helper::Failed => {
            return Err(ProbeError::Helper {
                what: "failed on a text sample",
            });
        }
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
        render::to_webp(media, input, kind, &output, fit)
            .await
            .map_err(|source| ProbeError::Render {
                what,
                vips: media.vips_command.clone(),
                source,
            })?;
    }
    Ok(())
}
