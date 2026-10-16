//! Blob persistence. Every state change is a conditional update.
//! `updated_at` is maintained by a database trigger, never by hand.

use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use sqlx::{PgConnection, PgExecutor, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use super::create::NewBlob;
use super::{
    Blob, BlobState, PreviewSource, Representation, RepresentationState, ScanOutcome, ScanState,
};
use crate::clamav::Verdict;
use crate::classify::Category;
use crate::jobs::{self, JobKind};

pub async fn insert(exec: impl PgExecutor<'_>, new: &NewBlob) -> sqlx::Result<Blob> {
    let scan_state = if new.skip_virus_scan {
        ScanState::Skipped
    } else {
        ScanState::Pending
    };
    sqlx::query_as::<_, Blob>(
        r#"
        INSERT INTO blobs (
            id, state, filename, content_type, category, input_type,
            checksum_sha256, size, skip_virus_scan, expires_at, scan_state
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        RETURNING *
        "#,
    )
    .bind(new.id)
    .bind(BlobState::Created.as_str())
    .bind(&new.filename)
    .bind(&new.content_type)
    .bind(new.category.as_str())
    .bind(new.input_type.as_str())
    .bind(&new.checksum_sha256)
    .bind(new.size)
    .bind(new.skip_virus_scan)
    .bind(new.expires_at.map(ToSqlx::to_sqlx))
    .bind(scan_state.as_str())
    .fetch_one(exec)
    .await
}

pub async fn find(exec: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<Option<Blob>> {
    sqlx::query_as::<_, Blob>("SELECT * FROM blobs WHERE id = $1")
        .bind(id)
        .fetch_optional(exec)
        .await
}

#[derive(Debug, Default, Clone)]
pub struct ListParams {
    pub content_type: Option<String>,
    pub category: Option<Category>,
    pub state: Option<BlobState>,
    pub scan_state: Option<ScanState>,
    pub cursor: Option<Uuid>,
    /// Already validated to lie in `1..=MAX_LIMIT`.
    pub limit: usize,
}

pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 200;

pub struct Page {
    pub items: Vec<Blob>,
    pub next_cursor: Option<Uuid>,
}

/// Newest first, keyset paginated on the UUID v7 id.
pub async fn list(exec: impl PgExecutor<'_>, params: &ListParams) -> sqlx::Result<Page> {
    let limit = params.limit;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT * FROM blobs WHERE true");
    if let Some(ct) = &params.content_type {
        qb.push(" AND content_type = ").push_bind(ct);
    }
    if let Some(category) = params.category {
        qb.push(" AND category = ").push_bind(category.as_str());
    }
    if let Some(state) = params.state {
        qb.push(" AND state = ").push_bind(state.as_str());
    }
    if let Some(scan_state) = params.scan_state {
        qb.push(" AND scan_state = ").push_bind(scan_state.as_str());
    }
    if let Some(cursor) = params.cursor {
        qb.push(" AND id < ").push_bind(cursor);
    }
    qb.push(" ORDER BY id DESC LIMIT ")
        .push_bind(limit as i64 + 1);

    let mut items: Vec<Blob> = qb.build_query_as().fetch_all(exec).await?;
    let next_cursor = if items.len() > limit {
        items.truncate(limit);
        items.last().map(|b| b.id)
    } else {
        None
    };
    Ok(Page { items, next_cursor })
}

/// Outcome of a conditional state change.
#[derive(Debug)]
pub enum Transition {
    Done(Blob),
    /// The blob exists but is not in an accepted source state.
    Conflict(Blob),
    Missing,
}

/// Move `id` from any of `from` to `to`, touching the matching timestamp.
pub async fn transition(
    conn: &mut PgConnection,
    id: Uuid,
    from: &[BlobState],
    to: BlobState,
) -> sqlx::Result<Transition> {
    let from: Vec<&str> = from.iter().map(|s| s.as_str()).collect();
    let timestamp_column = match to {
        BlobState::Uploading => Some("upload_started_at"),
        BlobState::Uploaded => Some("uploaded_at"),
        BlobState::Deleted => Some("deleted_at"),
        _ => None,
    };
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("UPDATE blobs SET state = ");
    qb.push_bind(to.as_str());
    if let Some(column) = timestamp_column {
        // The column name comes from the match above, never from input.
        qb.push(", ").push(column).push(" = now()");
    }
    qb.push(" WHERE id = ")
        .push_bind(id)
        .push(" AND state = ANY(")
        .push_bind(from)
        .push(") RETURNING *");
    let updated: Option<Blob> = qb.build_query_as().fetch_optional(&mut *conn).await?;
    match updated {
        Some(blob) => Ok(Transition::Done(blob)),
        None => match find(&mut *conn, id).await? {
            Some(blob) => Ok(Transition::Conflict(blob)),
            None => Ok(Transition::Missing),
        },
    }
}

/// `created` to `uploading`. Blobs are write-once, so no other state accepts
/// an upload.
pub async fn begin_upload(conn: &mut PgConnection, blob: &Blob) -> sqlx::Result<Transition> {
    transition(conn, blob.id, &[BlobState::Created], BlobState::Uploading).await
}

/// A failed upload hands the blob back to `created`: nothing was stored. The
/// stale-upload reaper applies the same rule.
pub async fn abort_upload(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Transition> {
    transition(conn, id, &[BlobState::Uploading], BlobState::Created).await
}

/// Everything an upload writes onto the row when it completes.
#[derive(Debug, Clone)]
pub struct Completed {
    pub storage_key: String,
    pub kek_id: String,
    pub wrapped_key: Vec<u8>,
    pub encryption_header: Vec<u8>,
    pub chunk_size: i32,
    pub checksum_sha256: Vec<u8>,
    pub size: i64,
    pub scan: ScanOutcome,
}

/// `uploading` to wherever the scan outcome leads: `uploaded` when a job
/// still has to scan, `ready` when clean or skipped, `infected` otherwise.
/// Points the row at the object and enqueues what comes next in the same
/// transaction.
pub async fn complete_upload(
    pool: &PgPool,
    blob: &Blob,
    done: &Completed,
) -> sqlx::Result<Transition> {
    let (state, scan_state) = done.scan.states();
    let mut tx = pool.begin().await?;
    let updated = sqlx::query_as::<_, Blob>(
        r#"
        UPDATE blobs
        SET state = $2, storage_key = $3,
            kek_id = $4, wrapped_key = $5, encryption_header = $6, chunk_size = $7,
            checksum_sha256 = $8, size = $9,
            scan_state = $10,
            scanned_at = CASE WHEN $11 THEN now() END,
            scan_signature = $13,
            uploaded_at = now()
        WHERE id = $1 AND state = $12
        RETURNING *
        "#,
    )
    .bind(blob.id)
    .bind(state.as_str())
    .bind(&done.storage_key)
    .bind(&done.kek_id)
    .bind(&done.wrapped_key)
    .bind(&done.encryption_header)
    .bind(done.chunk_size)
    .bind(&done.checksum_sha256)
    .bind(done.size)
    .bind(scan_state.as_str())
    .bind(done.scan.is_scanned())
    .bind(BlobState::Uploading.as_str())
    .bind(done.scan.signature())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(updated) = updated else {
        tx.rollback().await?;
        return match find(pool, blob.id).await? {
            Some(blob) => Ok(Transition::Conflict(blob)),
            None => Ok(Transition::Missing),
        };
    };
    enqueue_next(&mut tx, &updated).await?;
    tx.commit().await?;
    Ok(Transition::Done(updated))
}

/// The deferred scan's verdict: `uploaded` to `ready` or `infected`, with the
/// representations enqueued once the file is known clean.
pub async fn finish_scan(pool: &PgPool, id: Uuid, verdict: &Verdict) -> sqlx::Result<Transition> {
    let (state, scan_state, signature) = match verdict {
        Verdict::Clean => (BlobState::Ready, ScanState::Clean, None),
        Verdict::Infected(name) => (BlobState::Infected, ScanState::Infected, Some(name)),
    };
    let mut tx = pool.begin().await?;
    let updated = sqlx::query_as::<_, Blob>(
        "UPDATE blobs SET state = $2, scan_state = $3, scanned_at = now(), scan_signature = $5 \
         WHERE id = $1 AND state = $4 RETURNING *",
    )
    .bind(id)
    .bind(state.as_str())
    .bind(scan_state.as_str())
    .bind(BlobState::Uploaded.as_str())
    .bind(signature)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(updated) = updated else {
        tx.rollback().await?;
        return match find(pool, id).await? {
            Some(blob) => Ok(Transition::Conflict(blob)),
            None => Ok(Transition::Missing),
        };
    };
    enqueue_next(&mut tx, &updated).await?;
    tx.commit().await?;
    Ok(Transition::Done(updated))
}

/// What a blob that just changed state needs done: a scan while it waits in
/// `uploaded`, the representations once it is `ready`, nothing otherwise.
async fn enqueue_next(conn: &mut PgConnection, blob: &Blob) -> sqlx::Result<()> {
    let kinds: &[JobKind] = match blob.state {
        BlobState::Uploaded => &[JobKind::Scan],
        BlobState::Ready => &[JobKind::Render],
        _ => &[],
    };
    let now = Timestamp::now();
    for kind in kinds {
        jobs::enqueue(&mut *conn, *kind, Some(blob.id), serde_json::json!({}), now).await?;
    }
    Ok(())
}

/// Soft delete from any state, and enqueue a purge of the stored object at
/// `purge_at`.
pub async fn soft_delete(pool: &PgPool, id: Uuid, purge_at: Timestamp) -> sqlx::Result<Transition> {
    let mut tx = pool.begin().await?;
    let from: Vec<BlobState> = BlobState::ALL
        .iter()
        .copied()
        .filter(|s| *s != BlobState::Deleted)
        .collect();
    let result = transition(&mut tx, id, &from, BlobState::Deleted).await?;
    if let Transition::Done(blob) = &result
        && let Some(key) = &blob.storage_key
    {
        jobs::enqueue(
            &mut *tx,
            JobKind::Purge,
            None,
            serde_json::json!({ "blob_id": blob.id, "storage_key": key }),
            purge_at,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(result)
}

/// Record the outcome of rendering `kind` for `id`, along with the variant
/// version it was built with. Blob state is untouched: the representation
/// is written even if the blob was deleted meanwhile, and the purge takes
/// it with the rest of the prefix.
pub async fn finish_representation(
    exec: impl PgExecutor<'_>,
    id: Uuid,
    kind: Representation,
    state: RepresentationState,
    source: Option<PreviewSource>,
    version: i32,
) -> sqlx::Result<()> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("UPDATE blobs SET ");
    qb.push(kind.state_column())
        .push(" = ")
        .push_bind(state.as_str())
        .push(", ")
        .push(kind.version_column())
        .push(" = ")
        .push_bind(version);
    if kind == Representation::Preview {
        qb.push(", preview_source = ")
            .push_bind(source.map(PreviewSource::as_str));
    }
    qb.push(" WHERE id = ").push_bind(id);
    qb.build().execute(exec).await?;
    Ok(())
}

/// Ask for `kind` to be (re)generated: the representation goes back to
/// `pending` and a job is enqueued, if the blob is still `ready`. The
/// lazy path of the `GET` handler, for a missing object or a stale variant
/// version.
pub async fn request_representation(
    pool: &PgPool,
    id: Uuid,
    kind: Representation,
) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("UPDATE blobs SET ");
    qb.push(kind.state_column())
        .push(" = ")
        .push_bind(RepresentationState::Pending.as_str())
        .push(" WHERE id = ")
        .push_bind(id)
        .push(" AND state = ")
        .push_bind(BlobState::Ready.as_str());
    if qb.build().execute(&mut *tx).await?.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(false);
    }
    jobs::enqueue(
        &mut *tx,
        JobKind::Render,
        Some(id),
        serde_json::json!({}),
        Timestamp::now(),
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}
