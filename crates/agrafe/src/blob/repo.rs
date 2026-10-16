//! Blob persistence. Every state change is a conditional update.
//! `updated_at` is maintained by a database trigger, never by hand.

use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use sqlx::{PgConnection, PgExecutor, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use super::create::NewBlob;
use super::{Blob, BlobState, ScanState};
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
            checksum_sha256, size, write_once, skip_virus_scan, expires_at, scan_state
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
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
    .bind(new.write_once)
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

/// `created` (or `ready` for mutable blobs) to `uploading`.
pub async fn begin_upload(conn: &mut PgConnection, blob: &Blob) -> sqlx::Result<Transition> {
    let from: &[BlobState] = if blob.write_once {
        &[BlobState::Created]
    } else {
        &[BlobState::Created, BlobState::Ready]
    };
    transition(conn, blob.id, from, BlobState::Uploading).await
}

/// A failed upload hands the blob back to where it came from: `created` when
/// nothing was ever stored, `ready` when a mutable blob still has its previous
/// object. The stale-upload reaper applies the same rule.
pub async fn abort_upload(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Transition> {
    let updated = sqlx::query_as::<_, Blob>(
        r#"
        UPDATE blobs
        SET state = CASE WHEN storage_key IS NULL THEN $2 ELSE $3 END
        WHERE id = $1 AND state = $4
        RETURNING *
        "#,
    )
    .bind(id)
    .bind(BlobState::Created.as_str())
    .bind(BlobState::Ready.as_str())
    .bind(BlobState::Uploading.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    match updated {
        Some(blob) => Ok(Transition::Done(blob)),
        None => match find(&mut *conn, id).await? {
            Some(blob) => Ok(Transition::Conflict(blob)),
            None => Ok(Transition::Missing),
        },
    }
}

/// Everything an upload writes onto the row when it completes.
#[derive(Debug, Clone)]
pub struct Completed {
    pub storage_key: String,
    pub version: i32,
    pub kek_id: String,
    pub wrapped_key: Vec<u8>,
    pub encryption_header: Vec<u8>,
    pub chunk_size: i32,
    pub checksum_sha256: Vec<u8>,
    pub size: i64,
}

/// `uploading` to `uploaded`, or straight to `ready` when the scan is
/// skipped. Points the row at the new object, resets scan and representation
/// state, and enqueues what comes next in the same transaction: a scan, or
/// the representations, plus a purge at `purge_at` of the object this one
/// replaces.
pub async fn complete_upload(
    pool: &PgPool,
    blob: &Blob,
    done: &Completed,
    purge_at: Timestamp,
) -> sqlx::Result<Transition> {
    let (state, scan_state) = if blob.skip_virus_scan {
        (BlobState::Ready, ScanState::Skipped)
    } else {
        (BlobState::Uploaded, ScanState::Pending)
    };
    let mut tx = pool.begin().await?;
    let updated = sqlx::query_as::<_, Blob>(
        r#"
        UPDATE blobs
        SET state = $2, storage_key = $3, version = $4,
            kek_id = $5, wrapped_key = $6, encryption_header = $7, chunk_size = $8,
            checksum_sha256 = $9, size = $10,
            scan_state = $11, scanned_at = NULL,
            thumbnail_state = 'pending', preview_state = 'pending', preview_source = NULL,
            thumbnail_variant_version = NULL, preview_variant_version = NULL,
            uploaded_at = now()
        WHERE id = $1 AND state = $12
        RETURNING *
        "#,
    )
    .bind(blob.id)
    .bind(state.as_str())
    .bind(&done.storage_key)
    .bind(done.version)
    .bind(&done.kek_id)
    .bind(&done.wrapped_key)
    .bind(&done.encryption_header)
    .bind(done.chunk_size)
    .bind(&done.checksum_sha256)
    .bind(done.size)
    .bind(scan_state.as_str())
    .bind(BlobState::Uploading.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(updated) = updated else {
        tx.rollback().await?;
        return match find(pool, blob.id).await? {
            Some(blob) => Ok(Transition::Conflict(blob)),
            None => Ok(Transition::Missing),
        };
    };

    let now = Timestamp::now();
    let kinds: &[JobKind] = if updated.state == BlobState::Ready {
        &[JobKind::Thumbnail, JobKind::Preview]
    } else {
        &[JobKind::Scan]
    };
    for kind in kinds {
        jobs::enqueue(&mut *tx, *kind, Some(blob.id), serde_json::json!({}), now).await?;
    }
    if let Some(old) = &blob.storage_key {
        jobs::enqueue(
            &mut *tx,
            JobKind::Purge,
            None,
            serde_json::json!({ "blob_id": blob.id, "storage_key": old }),
            purge_at,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Transition::Done(updated))
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
