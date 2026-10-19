//! The deferred virus scan: stream the decrypted original to clamd and move
//! the blob out of `uploaded`.

use super::{Job, JobError};
use crate::app::Agrafe;
use crate::blob::BlobState;
use crate::blob::read::{Reader, Stored};
use crate::blob::repo::{self, Transition};
use crate::clamav::Verdict;

pub async fn run(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    let id = job.blob_id.ok_or(JobError::NoBlob(job.kind))?;
    let Some(blob) = repo::find(state.pool(), id).await? else {
        tracing::info!("blob is gone, nothing to scan");
        return Ok(());
    };
    if blob.state != BlobState::Uploaded {
        tracing::info!(state = %blob.state, "blob is not waiting for a scan");
        return Ok(());
    }

    let stored = Stored::from_blob(&blob)?;
    // clamd first: while it is down, every retry would otherwise open a
    // read on the object store for nothing.
    let mut session = state.clamd().begin().await?;
    let mut reader = Reader::open(state, &stored, 0..stored.size).await?;
    while let Some(plain) = reader.next_segment().await? {
        // clamd hangs up at its stream limit, or when it dies; fetching and
        // decrypting the rest would be for nothing.
        if !session.write(&plain).await {
            break;
        }
    }
    let verdict = session.finish().await?;

    match repo::finish_scan(state.pool(), &state.config().webhook, id, &verdict).await? {
        Transition::Done(blob) => match verdict {
            Verdict::Clean => tracing::info!(state = %blob.state, "blob is clean"),
            Verdict::Infected(name) => tracing::warn!(signature = name, "blob is infected"),
        },
        Transition::Conflict(blob) => {
            tracing::warn!(state = %blob.state, "blob changed state during the scan")
        }
        Transition::Missing => tracing::info!("blob was deleted during the scan"),
    }
    Ok(())
}
