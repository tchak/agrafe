//! Rendering a blob's thumbnail or preview: fetch and decrypt the original,
//! pick a source, rasterise it in a sandbox, store the WebP encrypted.
//!
//! A file the renderer rejects leaves the representation `failed` and the
//! job done: trying the same bytes again would end the same way. Only
//! failures of ours (store, database, a missing renderer) fail the job.

use std::path::Path;

use tokio::io::AsyncWriteExt;

use super::{Job, JobError, JobKind};
use crate::app::AppState;
use crate::blob::read::{Reader, Stored};
use crate::blob::{Blob, BlobState, PreviewSource, Representation, RepresentationState};
use crate::blob::{repo, representation};
use crate::media::render::{self, Fit};
use crate::media::sandbox::{self, Outcome};
use crate::media::{self, Source};

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

    let sources = media::sources(&blob.content_type);
    let too_large = blob
        .size
        .is_some_and(|size| size as u64 > media.max_source_size.as_u64());
    if sources.is_empty() || too_large {
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
    let original = workdir.path().join("original");
    download(state, &blob, &original).await?;

    let fit = match kind {
        Representation::Thumbnail => Fit::thumbnail(media),
        Representation::Preview => Fit::preview(media),
    };
    let output = workdir.path().join("rendered.webp");
    let mut rendered = None;
    let mut rejected = false;
    for source in sources {
        let (input, from) = match source {
            Source::Image => (original.clone(), PreviewSource::Image),
            Source::FirstPage => (original.clone(), PreviewSource::FirstPage),
            Source::EmbeddedImage => {
                let embedded = workdir.path().join("embedded");
                if !extract(state, &blob, &original, &embedded).await? {
                    continue;
                }
                (embedded, PreviewSource::EmbeddedImage)
            }
        };
        match render::to_webp(media, &input, &output, fit).await {
            Ok(bytes) => {
                rendered = Some((bytes, from));
                break;
            }
            Err(err) if err.is_input_fault() => {
                tracing::warn!(source = %from, error = %err, "rendering failed");
                rejected = true;
            }
            Err(err) => return Err(err.into()),
        }
    }

    let (rep_state, source) = match rendered {
        Some((bytes, from)) => {
            representation::store(state, &blob, kind, &bytes).await?;
            tracing::info!(source = %from, bytes = bytes.len(), "{kind} rendered");
            (RepresentationState::Ready, Some(from))
        }
        None if rejected => (RepresentationState::Failed, None),
        // Every source declined, as for a DOCX that is a real document.
        None => (RepresentationState::Unavailable, None),
    };
    repo::finish_representation(state.pool(), id, kind, rep_state, source, version).await?;
    Ok(())
}

/// Decrypt the original into `path`.
async fn download(state: &AppState, blob: &Blob, path: &Path) -> Result<(), JobError> {
    let stored = Stored::from_blob(blob)?;
    let mut reader = Reader::open(state, blob, &stored, 0..stored.size).await?;
    let mut file = tokio::fs::File::create(path).await?;
    while let Some(plain) = reader.next_segment().await? {
        file.write_all(&plain).await?;
    }
    file.flush().await?;
    Ok(())
}

/// Run the `agrafe extract` helper on `original`. `true` when it found an
/// embedded image and wrote it to `output`. A helper that fails or times
/// out means "no embedded image": the next source gets its turn.
async fn extract(
    state: &AppState,
    blob: &Blob,
    original: &Path,
    output: &Path,
) -> Result<bool, JobError> {
    let media = &state.config().media;
    let program = match &media.agrafe_command {
        Some(path) => path.clone(),
        None => std::env::current_exe()?,
    };
    let mut command = sandbox::command(program);
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
    match sandbox::run(command, &media.sandbox).await? {
        Outcome::Success => Ok(tokio::fs::try_exists(output).await?),
        Outcome::Failed { status, stderr } => {
            tracing::warn!(status, stderr, "the extraction helper failed");
            Ok(false)
        }
        Outcome::TimedOut => {
            tracing::warn!("the extraction helper timed out");
            Ok(false)
        }
    }
}
