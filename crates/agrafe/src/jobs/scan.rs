//! The deferred virus scan: stream the decrypted original to clamd and move
//! the blob out of `uploaded`.

use anyhow::Context as _;

use super::Job;
use crate::app::AppState;
use crate::blob::BlobState;
use crate::blob::read::{Reader, Stored};
use crate::blob::repo::{self, Transition};
use crate::clamav::Verdict;

pub async fn run(state: &AppState, job: &Job) -> anyhow::Result<()> {
    let id = job.blob_id.context("scan job has no blob")?;
    let Some(blob) = repo::find(state.pool(), id).await? else {
        tracing::info!("blob is gone, nothing to scan");
        return Ok(());
    };
    if blob.state != BlobState::Uploaded {
        tracing::info!(state = %blob.state, "blob is not waiting for a scan");
        return Ok(());
    }

    let stored = Stored::from_blob(&blob)?;
    let mut reader = Reader::open(state, &blob, &stored, 0..stored.size).await?;
    let mut session = state.clamd().begin().await?;
    while let Some(plain) = reader.next_segment().await? {
        session.write(&plain).await?;
    }
    let verdict = session.finish().await?;

    match repo::finish_scan(state.pool(), id, &verdict).await? {
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
