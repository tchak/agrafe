//! Remove a blob's objects from the bucket once nothing may read them
//! again: the original and the representations, everything under the
//! blob's prefix. Enqueued by a delete, after `upload.purge_delay`, and by
//! the reaper for blobs that expired or whose purge went missing.

use futures::TryStreamExt as _;
use object_store::ObjectStoreExt as _;
use object_store::path::Path as ObjectPath;
use uuid::Uuid;

use super::{Job, JobError};
use crate::app::Agrafe;
use crate::blob::{BlobState, repo};
use crate::storage::Store;

/// Why a purge could not run. A blob that is still readable is never
/// purged: the job is dead at once and stays in the table as evidence of
/// the bug that enqueued it.
#[derive(Debug, thiserror::Error)]
pub enum PurgeError {
    #[error("blob is {0}, not in a terminal state")]
    NotTerminal(BlobState),
    #[error("listing or deleting the objects: {0}")]
    Store(#[from] object_store::Error),
}

pub async fn run(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    let id = job.blob_id.ok_or(JobError::NoBlob(job.kind))?;
    if let Some(blob) = repo::find(state.pool(), id).await?
        && !blob.state.is_terminal()
    {
        return Err(PurgeError::NotTerminal(blob.state).into());
    }
    let objects = delete_prefix(state.store(), id)
        .await
        .map_err(PurgeError::Store)?;
    repo::mark_purged(state.pool(), id).await?;
    tracing::info!(objects, "blob purged");
    Ok(())
}

/// Delete every object under `<id>/`. Returns how many there were. An
/// object gone between the listing and its delete counts as deleted.
pub async fn delete_prefix(store: &Store, id: Uuid) -> Result<usize, object_store::Error> {
    let prefix = ObjectPath::from(id.to_string());
    let objects: Vec<_> = store.list(Some(&prefix)).try_collect().await?;
    for object in &objects {
        match store.delete(&object.location).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(objects.len())
}
