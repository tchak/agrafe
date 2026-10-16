//! Rendering a blob's thumbnail or preview: fetch and decrypt the original,
//! pick a source, rasterise it in a sandbox, store the WebP encrypted.
//!
//! A file the renderer rejects leaves the representation `failed` and the
//! job done: trying the same bytes again would end the same way. Only
//! failures of ours (store, database, a missing renderer) fail the job.

use std::ops::Range;
use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;

use super::{Job, JobError, JobKind};
use crate::app::AppState;
use crate::blob::read::{Reader, Stored};
use crate::blob::{Blob, BlobState, PreviewSource, Representation, RepresentationState};
use crate::blob::{repo, representation};
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

    let workdir = tempfile::Builder::new()
        .prefix("agrafe-")
        .tempdir_in(media.tmp_dir.clone().unwrap_or_else(std::env::temp_dir))?;
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
                if !extract(state, &blob, &whole, &embedded).await? {
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
                if !render_text(state, &sample, &page, fit).await? {
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

/// Run the `agrafe extract` helper on `original`. `true` when it found an
/// embedded image and wrote it to `output`.
async fn extract(
    state: &AppState,
    blob: &Blob,
    original: &Path,
    output: &Path,
) -> Result<bool, JobError> {
    let media = &state.config().media;
    let mut command = helper(state)?;
    command
        .arg("extract")
        .arg("--input")
        .arg(original)
        .arg("--output")
        .arg(output)
        .arg("--content-type")
        .arg(&blob.content_type)
        .arg("--min-coverage")
        .arg(media.embedded_image.min_coverage.to_string())
        .arg("--max-text-chars")
        .arg(media.embedded_image.max_text_chars.to_string())
        .arg("--max-output")
        .arg(media.sandbox.max_output_size.as_u64().to_string());
    run_helper(state, command, output, "extraction").await
}

/// Run the `agrafe render-text` helper on `sample`, drawing at twice the
/// box so the downscale leaves crisp glyphs. `true` when the sample was
/// text and a page was written to `output`.
async fn render_text(
    state: &AppState,
    sample: &Path,
    output: &Path,
    fit: Fit,
) -> Result<bool, JobError> {
    let mut command = helper(state)?;
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
    run_helper(state, command, output, "text").await
}

/// This binary, re-executed in the sandbox.
fn helper(state: &AppState) -> Result<tokio::process::Command, JobError> {
    let program = match &state.config().media.agrafe_command {
        Some(path) => path.clone(),
        None => std::env::current_exe()?,
    };
    Ok(sandbox::command(program))
}

/// A helper that succeeds without writing `output` found nothing to do.
/// One that fails or times out is treated the same: the next source gets
/// its turn.
async fn run_helper(
    state: &AppState,
    command: tokio::process::Command,
    output: &Path,
    what: &str,
) -> Result<bool, JobError> {
    match sandbox::run(command, &state.config().media.sandbox).await? {
        Outcome::Success => Ok(tokio::fs::try_exists(output).await?),
        Outcome::Failed { status, stderr } => {
            tracing::warn!(status, stderr, "the {what} helper failed");
            Ok(false)
        }
        Outcome::TimedOut => {
            tracing::warn!("the {what} helper timed out");
            Ok(false)
        }
    }
}
