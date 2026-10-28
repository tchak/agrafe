//! Remove a blob's objects from the bucket once nothing may read them
//! again: the original and the representations, everything under the
//! blob's prefix. Enqueued by a delete, after `upload.purge_delay`, and by
//! the reaper for blobs that expired or whose purge went missing.

use futures::TryStreamExt as _;
use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use object_store::ObjectStoreExt as _;
use object_store::path::Path as ObjectPath;
use sqlx::PgExecutor;
use uuid::Uuid;

use super::{Job, JobError, JobKind, Priority};
use crate::app::Agrafe;
use crate::blob::{BlobState, key, repo};
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

/// Enqueue a purge at `run_at` for each of `blob_ids` that needs one as of
/// `now`, in one statement: an `expired` or `deleted` blob, or an
/// `infected` one past its expiry, that still holds objects
/// (`Blob::holds_objects`). A blob with a purge already pending is left
/// with it. Returns how many were enqueued.
pub(crate) async fn enqueue_each(
    exec: impl PgExecutor<'_>,
    blob_ids: &[Uuid],
    run_at: Timestamp,
    now: Timestamp,
) -> sqlx::Result<u64> {
    if blob_ids.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query(
        "WITH inserted AS (\
             INSERT INTO jobs (kind, blob_id, priority, run_at, traceparent) \
             SELECT $1, b.id, $3, $4, $6 FROM blobs b \
             WHERE b.id = ANY($2) AND b.purged_at IS NULL \
               AND (b.storage_key IS NOT NULL OR b.source_parts > 0 \
                    OR b.upload_started_at IS NOT NULL) \
               AND (b.state IN ('expired', 'deleted') \
                    OR (b.state = 'infected' AND b.expires_at <= $7)) \
             ON CONFLICT DO NOTHING RETURNING kind\
         ) \
         SELECT pg_notify($5, kind) FROM inserted",
    )
    .bind(JobKind::Purge.as_str())
    .bind(blob_ids)
    .bind(Priority::Normal.rank())
    .bind(run_at.to_sqlx())
    .bind(super::CHANNEL)
    .bind(super::current_traceparent())
    .bind(now.to_sqlx())
    .execute(exec)
    .await?;
    Ok(result.rows_affected())
}

/// Delete every object under the blob's prefix (`key::prefix`). Returns
/// how many there were. An object gone between the listing and its delete
/// counts as deleted.
pub async fn delete_prefix(store: &Store, id: Uuid) -> Result<usize, object_store::Error> {
    let prefix = ObjectPath::from(key::prefix(id));
    let objects: Vec<_> = store.list(Some(&prefix)).try_collect().await?;
    for object in &objects {
        match store.delete(&object.location).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(objects.len())
}
