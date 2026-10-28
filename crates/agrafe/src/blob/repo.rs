//! Blob persistence. Every state change is a conditional update.
//! `updated_at` is maintained by a database trigger, never by hand.

use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use sqlx::{AssertSqlSafe, PgConnection, PgExecutor, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use super::create::NewBlob;
use super::{
    Blob, BlobHead, BlobState, InputType, PreviewSource, Rendered, Representation,
    RepresentationState, ScanOutcome, ScanState,
};
use crate::attachment;
use crate::clamav::Verdict;
use crate::classify::Category;
use crate::config::WebhookConfig;
use crate::encryption::Envelope;
use crate::jobs::notify::{self, Event};
use crate::jobs::{self, JobKind, Priority};
use crate::source::rows::Sheet;

/// The columns `Blob` reads, as SQL: every one of the table but
/// `source_columns`, a `rows` blob's declaration, up to 16 MiB of JSON that
/// only the part's accept and the build read, through `sheets`. A listing
/// page of such blobs would otherwise fetch and decode two hundred of
/// them. A macro, so that the queries stay literals.
macro_rules! blob_columns {
    () => {
        "id, state, filename, content_type, category, input_type, \
         checksum_sha256, checksum_md5, size, skip_virus_scan, url, expires_at, priority, \
         scan_state, scanned_at, scan_signature, storage_key, \
         source_parts, source_rows, source_size, source_updated_at, source_sheet, \
         source_sheet_rows, archive_entries, archive_errors, source_main, workspace_size, \
         kek_id, wrapped_key, encryption_header, chunk_size, \
         thumbnail_state, preview_state, preview_source, \
         thumbnail_variant_version, preview_variant_version, \
         thumbnail_sha256, thumbnail_size, preview_sha256, preview_size, \
         created_at, updated_at, upload_started_at, upload_heartbeat_at, uploaded_at, \
         deleted_at, purged_at, failed_at, last_error"
    };
}
pub(crate) use blob_columns;

/// [`blob_columns!`], for the test that holds it to the table.
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub const BLOB_COLUMNS: &str = blob_columns!();

/// The sheets of a `rows` blob in workbook order, each with its columns in
/// order, as declared at creation; `None` for a blob without them or that
/// does not exist.
pub(crate) async fn sheets(
    exec: impl PgExecutor<'_>,
    id: Uuid,
) -> sqlx::Result<Option<Vec<Sheet>>> {
    let sheets: Option<Option<sqlx::types::Json<Vec<Sheet>>>> =
        sqlx::query_scalar("SELECT source_columns FROM blobs WHERE id = $1")
            .bind(id)
            .fetch_optional(exec)
            .await?;
    Ok(sheets
        .flatten()
        .map(|sheets| sheets.0)
        .filter(|sheets| !sheets.is_empty()))
}

pub(crate) async fn insert(exec: impl PgExecutor<'_>, new: &NewBlob) -> sqlx::Result<Blob> {
    let scan_state = if new.skip_virus_scan {
        ScanState::Skipped
    } else {
        ScanState::Pending
    };
    let initial = |kind| {
        if new.representations.contains(&kind) {
            RepresentationState::Pending
        } else {
            RepresentationState::Disabled
        }
    };
    sqlx::query_as::<_, Blob>(concat!(
        r#"
        INSERT INTO blobs (
            id, state, filename, content_type, category, input_type,
            checksum_sha256, size, skip_virus_scan, expires_at, scan_state, priority,
            source_columns, thumbnail_state, preview_state, checksum_md5, url, source_main
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
        RETURNING "#,
        blob_columns!()
    ))
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
    .bind(new.priority.rank())
    .bind(new.source_columns.as_ref().map(sqlx::types::Json))
    .bind(initial(Representation::Thumbnail).as_str())
    .bind(initial(Representation::Preview).as_str())
    .bind(&new.checksum_md5)
    .bind(&new.url)
    .bind(&new.source_main)
    .fetch_one(exec)
    .await
}

pub async fn find(exec: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<Option<Blob>> {
    sqlx::query_as::<_, Blob>(concat!(
        "SELECT ",
        blob_columns!(),
        " FROM blobs WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(exec)
    .await
}

/// The one query on the read path: the columns of a [`BlobHead`], listed
/// so a download never ships `source_columns` or the rest of the row.
pub(crate) async fn find_head(
    exec: impl PgExecutor<'_>,
    id: Uuid,
) -> sqlx::Result<Option<BlobHead>> {
    sqlx::query_as::<_, BlobHead>(
        "SELECT id, state, filename, content_type, category, checksum_sha256, size, \
         expires_at, scan_state, storage_key, kek_id, wrapped_key, encryption_header, \
         chunk_size, thumbnail_state, preview_state, thumbnail_variant_version, \
         preview_variant_version, thumbnail_sha256, thumbnail_size, preview_sha256, \
         preview_size, uploaded_at \
         FROM blobs WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(exec)
    .await
}

/// The blobs among `ids` that exist, in no particular order.
pub(crate) async fn find_many(exec: impl PgExecutor<'_>, ids: &[Uuid]) -> sqlx::Result<Vec<Blob>> {
    sqlx::query_as::<_, Blob>(concat!(
        "SELECT ",
        blob_columns!(),
        " FROM blobs WHERE id = ANY($1)"
    ))
    .bind(ids)
    .fetch_all(exec)
    .await
}

#[derive(Debug, Default, Clone)]
pub struct ListParams {
    pub content_type: Option<String>,
    pub category: Option<Category>,
    pub state: Option<BlobState>,
    pub scan_state: Option<ScanState>,
    /// Whether any attachment exists for the blob.
    pub attached: Option<bool>,
    /// Whether the job that was to finish the blob died (`failed_at`).
    pub failed: Option<bool>,
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
pub(crate) async fn list(exec: impl PgExecutor<'_>, params: &ListParams) -> sqlx::Result<Page> {
    let limit = params.limit;
    let mut qb = list_query(
        concat!("SELECT ", blob_columns!(), " FROM blobs WHERE true"),
        params,
    );
    let mut items: Vec<Blob> = qb.build_query_as().fetch_all(exec).await?;
    let next_cursor = if items.len() > limit {
        items.truncate(limit);
        items.last().map(|b| b.id)
    } else {
        None
    };
    Ok(Page { items, next_cursor })
}

/// The listing's query after `head`, which selects from `blobs` and ends
/// on a condition. A filter on a partial index is spelled as a literal: a
/// bound value would leave a cached generic plan unable to prove the
/// index's predicate.
fn list_query(head: &str, params: &ListParams) -> QueryBuilder<Postgres> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(head);
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
    if let Some(attached) = params.attached {
        // Attach clears `expires_at` and the last detach sets it again, so
        // a blob is attached exactly when it has none, terminal or not
        // (DESIGN.md, "Attachments"). The row answers on its own index;
        // asking `attachments` would be an anti-join no index serves.
        qb.push(if attached {
            " AND expires_at IS NULL"
        } else {
            " AND expires_at IS NOT NULL"
        });
    }
    if let Some(failed) = params.failed {
        qb.push(if failed {
            " AND failed_at IS NOT NULL"
        } else {
            " AND failed_at IS NULL"
        });
    }
    if let Some(cursor) = params.cursor {
        qb.push(" AND id < ").push_bind(cursor);
    }
    qb.push(" ORDER BY id DESC LIMIT ")
        .push_bind(params.limit as i64 + 1);
    qb
}

/// The plan of the listing's query for `params`, as `EXPLAIN` prints it,
/// for tests that pin the listing's filters to their indexes.
#[cfg(any(test, feature = "test-support"))]
pub async fn explain_list(exec: impl PgExecutor<'_>, params: &ListParams) -> sqlx::Result<String> {
    let mut qb = list_query(
        concat!("EXPLAIN SELECT ", blob_columns!(), " FROM blobs WHERE true"),
        params,
    );
    let lines: Vec<(String,)> = qb.build_query_as().fetch_all(exec).await?;
    Ok(lines
        .into_iter()
        .map(|(l,)| l)
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Outcome of a conditional state change.
#[derive(Debug)]
pub enum Transition {
    Done(Blob),
    /// The blob exists but is not in an accepted source state.
    Conflict(Blob),
    Missing,
}

/// What a terminal transition appends: nothing is going to finish the
/// blob, so the verdict of the job that was to is moot. `expire` and
/// `expire_abandoned` spell the same out.
const CLEAR_FAILURE: &str = ", failed_at = NULL, last_error = NULL";

/// Move `id` from any of `from` to `to`, touching the matching timestamp.
/// A terminal `to` clears the failure recorded on the row.
pub async fn transition(
    conn: &mut PgConnection,
    id: Uuid,
    from: &[BlobState],
    to: BlobState,
) -> sqlx::Result<Transition> {
    let from: Vec<&str> = from.iter().map(|s| s.as_str()).collect();
    let timestamp_columns: &[&str] = match to {
        BlobState::Uploading => &["upload_started_at", "upload_heartbeat_at"],
        BlobState::Uploaded => &["uploaded_at"],
        BlobState::Deleted => &["deleted_at"],
        _ => &[],
    };
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("UPDATE blobs SET state = ");
    qb.push_bind(to.as_str());
    for column in timestamp_columns {
        // The column names come from the match above, never from input.
        qb.push(", ").push(column).push(" = now()");
    }
    if to.is_terminal() {
        qb.push(CLEAR_FAILURE);
    }
    qb.push(" WHERE id = ")
        .push_bind(id)
        .push(" AND state = ANY(")
        .push_bind(from)
        .push(concat!(") RETURNING ", blob_columns!()));
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

/// `created` to `uploading` for source part `n` of a generated blob, which
/// must be the next one: the sequence check is in the same statement as
/// the transition, so two uploads of the same part cannot both win. The
/// first part fixes the blob's data key and segment layout, which every
/// part and the built original then share; `envelope` is what to record
/// when the row has none yet.
pub(crate) async fn begin_part(
    conn: &mut PgConnection,
    blob: &Blob,
    n: u32,
    envelope: &Envelope,
    chunk_size: i32,
) -> sqlx::Result<Transition> {
    let updated = sqlx::query_as::<_, Blob>(concat!(
        "UPDATE blobs SET state = $2, upload_started_at = now(), upload_heartbeat_at = now(), \
             kek_id = COALESCE(kek_id, $5), wrapped_key = COALESCE(wrapped_key, $6), \
             chunk_size = COALESCE(chunk_size, $7) \
         WHERE id = $1 AND state = $3 AND source_parts = $4 RETURNING ",
        blob_columns!()
    ))
    .bind(blob.id)
    .bind(BlobState::Uploading.as_str())
    .bind(BlobState::Created.as_str())
    .bind(n as i32 - 1)
    .bind(&envelope.kek_id)
    .bind(&envelope.wrapped)
    .bind(chunk_size)
    .fetch_optional(&mut *conn)
    .await?;
    match updated {
        Some(blob) => Ok(Transition::Done(blob)),
        None => match find(&mut *conn, blob.id).await? {
            Some(blob) => Ok(Transition::Conflict(blob)),
            None => Ok(Transition::Missing),
        },
    }
}

/// A failed upload hands the blob back to `created`: nothing was stored. The
/// stale-upload reaper applies the same rule. `started` is the upload's
/// `upload_started_at`, its proof of ownership: once the reaper has reset
/// the row and another upload has taken it, this one must not touch it.
pub async fn abort_upload(
    conn: &mut PgConnection,
    id: Uuid,
    started: Timestamp,
) -> sqlx::Result<Transition> {
    let updated = sqlx::query_as::<_, Blob>(concat!(
        "UPDATE blobs SET state = $2 \
         WHERE id = $1 AND state = $3 AND upload_started_at = $4 RETURNING ",
        blob_columns!()
    ))
    .bind(id)
    .bind(BlobState::Created.as_str())
    .bind(BlobState::Uploading.as_str())
    .bind(started.to_sqlx())
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

/// Sign of life from an upload in flight. Returns whether the blob is still
/// `uploading` under this upload, `started` being its `upload_started_at`:
/// the reaper may have reset it, a delete taken it, or another upload
/// begun since.
pub async fn touch_upload(
    exec: impl PgExecutor<'_>,
    id: Uuid,
    started: Timestamp,
) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "UPDATE blobs SET upload_heartbeat_at = now() \
         WHERE id = $1 AND state = $2 AND upload_started_at = $3",
    )
    .bind(id)
    .bind(BlobState::Uploading.as_str())
    .bind(started.to_sqlx())
    .execute(exec)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Everything an upload writes onto the row when it completes.
#[derive(Debug, Clone)]
pub(crate) struct Completed {
    pub storage_key: String,
    pub kek_id: String,
    pub wrapped_key: Vec<u8>,
    pub encryption_header: Vec<u8>,
    pub chunk_size: i32,
    pub checksum_sha256: Vec<u8>,
    pub checksum_md5: Vec<u8>,
    pub size: i64,
    pub scan: ScanOutcome,
    /// A content type more precise than the declared one, read off the
    /// bytes; `None` keeps the declaration. It never changes the category.
    pub content_type: Option<String>,
}

/// `uploading` to wherever the scan outcome leads: `uploaded` when a job
/// still has to scan, `ready` when clean or skipped, `infected` otherwise.
/// Points the row at the object and enqueues what comes next in the same
/// transaction. Only the upload that began it, identified by `started`,
/// its `upload_started_at`, may complete it.
pub(crate) async fn complete_upload(
    pool: &PgPool,
    webhook: &WebhookConfig,
    blob: &Blob,
    started: Timestamp,
    done: &Completed,
) -> sqlx::Result<Transition> {
    let (state, scan_state) = done.scan.states();
    let mut tx = pool.begin().await?;
    let updated = sqlx::query_as::<_, Blob>(concat!(
        r#"
        UPDATE blobs
        SET state = $2, storage_key = $3,
            kek_id = $4, wrapped_key = $5, encryption_header = $6, chunk_size = $7,
            checksum_sha256 = $8, size = $9,
            scan_state = $10,
            scanned_at = CASE WHEN $11 THEN now() END,
            scan_signature = $13,
            uploaded_at = now(),
            checksum_md5 = $15,
            content_type = COALESCE($16, content_type),
            -- The presigned URL of a fetched blob served its purpose: a
            -- retry only ever reads it while the blob is created.
            url = NULL
        WHERE id = $1 AND state = $12 AND upload_started_at = $14
        RETURNING "#,
        blob_columns!()
    ))
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
    .bind(started.to_sqlx())
    .bind(&done.checksum_md5)
    .bind(&done.content_type)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(updated) = updated else {
        tx.rollback().await?;
        return match find(pool, blob.id).await? {
            Some(blob) => Ok(Transition::Conflict(blob)),
            None => Ok(Transition::Missing),
        };
    };
    enqueue_next(&mut tx, webhook, &updated).await?;
    tx.commit().await?;
    Ok(Transition::Done(updated))
}

/// What a part adds to its blob's source once stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PartAccepted {
    /// Lines in the part.
    pub rows: u64,
    /// Bytes in the part.
    pub bytes: u64,
    /// What the part adds to the size of the generated file, when the type
    /// knows it at accept time: the members of an archive.
    pub size: Option<u64>,
    /// Where the part left a `rows` source: the sheet it is filling and the
    /// lines in it so far. `None` leaves the row as it is.
    pub sheet: Option<SheetPosition>,
    /// What the part adds to the workspace of a `document` blob, in bytes.
    pub workspace: u64,
}

/// The position of a `rows` source within its workbook, carried across
/// parts because the row limit is per sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SheetPosition {
    /// Index of the sheet, from zero.
    pub index: u32,
    /// Lines accepted into it so far.
    pub rows: u64,
}

/// Part `n` is stored: `uploading` back to `created`, the part counted on
/// the row. Only the upload that began it, identified by `started`, may
/// record it.
pub(crate) async fn complete_part(
    conn: &mut PgConnection,
    id: Uuid,
    started: Timestamp,
    n: u32,
    accepted: &PartAccepted,
) -> sqlx::Result<Transition> {
    let updated = sqlx::query_as::<_, Blob>(concat!(
        "UPDATE blobs \
         SET state = $2, source_parts = $4, source_rows = source_rows + $5, \
             source_size = source_size + $6, \
             size = CASE WHEN $7::bigint IS NULL THEN size ELSE COALESCE(size, 0) + $7 END, \
             source_sheet = COALESCE($9, source_sheet), \
             source_sheet_rows = COALESCE($10, source_sheet_rows), \
             workspace_size = workspace_size + $11, \
             source_updated_at = now() \
         WHERE id = $1 AND state = $3 AND upload_started_at = $8 RETURNING ",
        blob_columns!()
    ))
    .bind(id)
    .bind(BlobState::Created.as_str())
    .bind(BlobState::Uploading.as_str())
    .bind(n as i32)
    .bind(accepted.rows as i64)
    .bind(accepted.bytes as i64)
    .bind(accepted.size.map(|s| s as i64))
    .bind(started.to_sqlx())
    .bind(accepted.sheet.map(|p| p.index as i32))
    .bind(accepted.sheet.map(|p| p.rows as i64))
    .bind(accepted.workspace as i64)
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

/// Seal a source: `created` to `uploaded`, with the build job enqueued in
/// the same transaction. Refused unless the blob is a generated one holding
/// at least one part; the caller reads the reason off the returned row.
pub(crate) async fn finish_source(
    pool: &PgPool,
    webhook: &WebhookConfig,
    id: Uuid,
) -> sqlx::Result<Transition> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query_as::<_, Blob>(concat!(
        "UPDATE blobs SET state = $2, uploaded_at = now() \
         WHERE id = $1 AND state = $3 AND source_parts > 0 AND input_type = ANY($4) \
         RETURNING ",
        blob_columns!()
    ))
    .bind(id)
    .bind(BlobState::Uploaded.as_str())
    .bind(BlobState::Created.as_str())
    .bind(
        InputType::ALL
            .iter()
            .filter(|t| t.is_generated())
            .map(|t| t.as_str())
            .collect::<Vec<_>>(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(updated) = updated else {
        tx.rollback().await?;
        return match find(pool, id).await? {
            Some(blob) => Ok(Transition::Conflict(blob)),
            None => Ok(Transition::Missing),
        };
    };
    enqueue_next(&mut tx, webhook, &updated).await?;
    tx.commit().await?;
    Ok(Transition::Done(updated))
}

/// What a build writes onto the row when the generated file is stored.
#[derive(Debug, Clone)]
pub(crate) struct Built {
    pub storage_key: String,
    pub encryption_header: Vec<u8>,
    pub checksum_sha256: Vec<u8>,
    pub checksum_md5: Vec<u8>,
    pub size: i64,
    /// Members written and members skipped, for an archive.
    pub archive: Option<(i64, i64)>,
}

/// The build is done: `uploaded` to `ready`, the row pointed at the object
/// with its final checksum and size, the representations enqueued. The
/// data key and layout were fixed by the first part and are not touched.
pub(crate) async fn complete_build(
    pool: &PgPool,
    webhook: &WebhookConfig,
    id: Uuid,
    built: &Built,
) -> sqlx::Result<Transition> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query_as::<_, Blob>(concat!(
        "UPDATE blobs \
         SET state = $2, storage_key = $4, encryption_header = $5, checksum_sha256 = $6, \
             size = $7, archive_entries = $8, archive_errors = $9, checksum_md5 = $10 \
         WHERE id = $1 AND state = $3 RETURNING ",
        blob_columns!()
    ))
    .bind(id)
    .bind(BlobState::Ready.as_str())
    .bind(BlobState::Uploaded.as_str())
    .bind(&built.storage_key)
    .bind(&built.encryption_header)
    .bind(&built.checksum_sha256)
    .bind(built.size)
    .bind(built.archive.map(|(entries, _)| entries))
    .bind(built.archive.map(|(_, errors)| errors))
    .bind(&built.checksum_md5)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(updated) = updated else {
        tx.rollback().await?;
        return match find(pool, id).await? {
            Some(blob) => Ok(Transition::Conflict(blob)),
            None => Ok(Transition::Missing),
        };
    };
    enqueue_next(&mut tx, webhook, &updated).await?;
    tx.commit().await?;
    Ok(Transition::Done(updated))
}

/// The deferred scan's verdict: `uploaded` to `ready` or `infected`, with the
/// representations enqueued once the file is known clean.
pub(crate) async fn finish_scan(
    pool: &PgPool,
    webhook: &WebhookConfig,
    id: Uuid,
    verdict: &Verdict,
) -> sqlx::Result<Transition> {
    let (state, scan_state, signature) = match verdict {
        Verdict::Clean => (BlobState::Ready, ScanState::Clean, None),
        Verdict::Infected(name) => (BlobState::Infected, ScanState::Infected, Some(name)),
    };
    let mut tx = pool.begin().await?;
    let updated = sqlx::query_as::<_, Blob>(concat!(
        "UPDATE blobs SET state = $2, scan_state = $3, scanned_at = now(), scan_signature = $5 \
         WHERE id = $1 AND state = $4 RETURNING ",
        blob_columns!()
    ))
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
    enqueue_next(&mut tx, webhook, &updated).await?;
    tx.commit().await?;
    Ok(Transition::Done(updated))
}

/// What a blob that just changed state needs done: the fetch of a `url`
/// blob while it is `created`; while it waits in `uploaded`, a scan for a
/// file or the build for a source; the representations once it is
/// `ready`, unless both were disabled at creation; nothing otherwise. And
/// the news of it: a `ready` or `infected` the webhook wants is enqueued
/// here too, in the same transaction.
pub(crate) async fn enqueue_next(
    conn: &mut PgConnection,
    webhook: &WebhookConfig,
    blob: &Blob,
) -> sqlx::Result<()> {
    enqueue_kinds(conn, blob, next_kinds(blob)).await?;
    if let Some(event) = Event::of(blob).filter(|event| webhook.wants(*event)) {
        notify::enqueue(&mut *conn, blob.id, blob.priority, event, Timestamp::now()).await?;
    }
    Ok(())
}

/// The job kinds a blob in its current state is waiting on; empty when it
/// waits on none.
fn next_kinds(blob: &Blob) -> &'static [JobKind] {
    let pending = |kind| blob.representation_state(kind) == RepresentationState::Pending;
    match (blob.state, blob.input_type) {
        (BlobState::Created, InputType::Url) => &[JobKind::Fetch],
        (BlobState::Uploaded, InputType::Raw | InputType::Multipart | InputType::Url) => {
            &[JobKind::Scan]
        }
        (BlobState::Uploaded, InputType::Rows) => &[JobKind::Rows],
        (BlobState::Uploaded, InputType::Archive) => &[JobKind::Archive],
        (BlobState::Uploaded, InputType::Document) => &[JobKind::Document],
        (BlobState::Ready, _)
            if pending(Representation::Thumbnail) || pending(Representation::Preview) =>
        {
            &[JobKind::Render]
        }
        _ => &[],
    }
}

/// Enqueue one job of each of `kinds` for the blob, due now, at the blob's
/// priority. Returns the kinds a row was inserted for: a kind with a job
/// pending already is left to it.
async fn enqueue_kinds(
    conn: &mut PgConnection,
    blob: &Blob,
    kinds: &[JobKind],
) -> sqlx::Result<Vec<JobKind>> {
    let now = Timestamp::now();
    let mut inserted = Vec::new();
    for kind in kinds {
        if jobs::enqueue(
            &mut *conn,
            *kind,
            Some(blob.id),
            blob.priority,
            serde_json::json!({}),
            now,
        )
        .await?
        {
            inserted.push(*kind);
        }
    }
    Ok(inserted)
}

/// Outcome of `retry`.
#[derive(Debug)]
pub(crate) enum Retry {
    /// Jobs of these kinds were enqueued; the blob as it is after.
    Enqueued(Blob, Vec<JobKind>),
    /// The blob waits on no job in its state as of `now`, terminal included.
    Nothing(Blob),
    /// Every job the blob waits on is pending already.
    Pending,
    Missing,
}

/// Enqueue whatever the blob is waiting on, from the blob's side: the
/// fetch of a `created` `url` blob, the scan or the build of an `uploaded`
/// blob, the render of a `ready` one with a representation `pending`. Dead jobs of those kinds for the blob are
/// removed on the way, superseded; a pending one is left to do the work.
/// The row is locked so a concurrent transition cannot slip in between
/// the reading of its state and the enqueue.
pub(crate) async fn retry(pool: &PgPool, id: Uuid, now: Timestamp) -> sqlx::Result<Retry> {
    let mut tx = pool.begin().await?;
    let blob = sqlx::query_as::<_, Blob>(concat!(
        "SELECT ",
        blob_columns!(),
        " FROM blobs WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(blob) = blob else {
        return Ok(Retry::Missing);
    };
    if blob.state_at(now).is_terminal() {
        return Ok(Retry::Nothing(blob));
    }
    let kinds = next_kinds(&blob);
    if kinds.is_empty() {
        return Ok(Retry::Nothing(blob));
    }
    let kind_names: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
    sqlx::query("DELETE FROM jobs WHERE blob_id = $1 AND failed_at IS NOT NULL AND kind = ANY($2)")
        .bind(id)
        .bind(&kind_names)
        .execute(&mut *tx)
        .await?;
    // A job that is running holds no pending slot, so the enqueue alone
    // would give it a twin doing the same work; it is left to finish.
    let live: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT kind FROM jobs \
         WHERE blob_id = $1 AND failed_at IS NULL AND kind = ANY($2)",
    )
    .bind(id)
    .bind(&kind_names)
    .fetch_all(&mut *tx)
    .await?;
    let waiting: Vec<JobKind> = kinds
        .iter()
        .copied()
        .filter(|kind| !live.iter().any(|l| l == kind.as_str()))
        .collect();
    let inserted = enqueue_kinds(&mut tx, &blob, &waiting).await?;
    if inserted.is_empty() {
        return Ok(Retry::Pending);
    }
    // The enqueue cleared the failure on the row; read it back as it is.
    let blob = sqlx::query_as::<_, Blob>(concat!(
        "SELECT ",
        blob_columns!(),
        " FROM blobs WHERE id = $1"
    ))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Retry::Enqueued(blob, inserted))
}

/// Outcome of a soft delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delete {
    /// Deleted, or deleted already.
    Done,
    /// The blob has attachments, and was left alone.
    Attached,
    Missing,
}

/// Soft delete from any state but attached, and enqueue a purge of the
/// stored objects at `purge_at`. The row is locked before the attachments
/// are counted, so an attach cannot slip in between the check and the
/// transition.
pub(crate) async fn soft_delete(
    pool: &PgPool,
    id: Uuid,
    purge_at: Timestamp,
) -> sqlx::Result<Delete> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> = sqlx::query_scalar("SELECT id FROM blobs WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    if locked.is_none() {
        return Ok(Delete::Missing);
    }
    if attachment::exists_for_blob(&mut *tx, id).await? {
        return Ok(Delete::Attached);
    }
    let from: Vec<BlobState> = BlobState::ALL
        .iter()
        .copied()
        .filter(|s| *s != BlobState::Deleted)
        .collect();
    let result = transition(&mut tx, id, &from, BlobState::Deleted).await?;
    if let Transition::Done(blob) = &result
        && blob.holds_objects()
        && blob.purged_at.is_none()
    {
        // Housekeeping, not something a user waits on: a purge runs at
        // `normal` whatever the blob's priority, as the reaper's do.
        jobs::enqueue(
            &mut *tx,
            JobKind::Purge,
            Some(blob.id),
            Priority::Normal,
            serde_json::json!({}),
            purge_at,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(match result {
        Transition::Missing => Delete::Missing,
        // `from` is every state but `deleted`, so a conflict is a blob
        // deleted already, which is not an error.
        Transition::Done(_) | Transition::Conflict(_) => Delete::Done,
    })
}

/// Record as `expired` the non-terminal blobs past their `expires_at` at
/// `now`, at most `limit` of them, the longest due first. The reads
/// already treat them so; this only writes it down.
pub async fn expire(
    conn: &mut PgConnection,
    now: Timestamp,
    limit: i64,
) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(AssertSqlSafe(expire_sql(limit)))
        .bind(BlobState::Expired.as_str())
        .bind(now.to_sqlx())
        .fetch_all(&mut *conn)
        .await
}

/// `expire`'s statement: `$1` the state, `$2` the time. The terminal
/// states are spelled out so that `blobs_expires_at_idx` applies, see
/// `state_literals`.
///
/// The reaper's statements take a batch the same way: the ids, picked by
/// the statement's index in its order and limited, in an array the update
/// then looks up by primary key, its condition repeated. An update that
/// waits on a row lock, such as an attach's, evaluates its own condition
/// again on the row as it then is, which a subquery's would not; and the
/// array keeps the planner from joining the whole table to the batch.
fn expire_sql(limit: i64) -> String {
    let due = format!(
        "expires_at <= $2 AND state NOT IN ({})",
        state_literals(BlobState::ALL.iter().copied().filter(|s| s.is_terminal()))
    );
    format!(
        "UPDATE blobs SET state = $1{CLEAR_FAILURE} \
         WHERE id = ANY(ARRAY(\
             SELECT id FROM blobs WHERE {due} ORDER BY expires_at LIMIT {limit}\
         )) AND {due} \
         RETURNING id"
    )
}

/// Expire every `created` blob that was abandoned before `before`: one
/// holding source parts whose last part landed before then, an export the
/// backend never finished, and an attached one with no parts that nothing
/// has touched since then, a file that never arrived. An unattached blob
/// with no parts is left to its own `expires_at`, which it always has.
/// Whatever parts there are go with the purge of the prefix.
pub(crate) async fn expire_abandoned(
    conn: &mut PgConnection,
    before: Timestamp,
    limit: i64,
) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(AssertSqlSafe(expire_abandoned_sql(limit)))
        .bind(BlobState::Expired.as_str())
        .bind(before.to_sqlx())
        .fetch_all(&mut *conn)
        .await
}

/// `expire_abandoned`'s statement: `$1` the state, `$2` the time. The
/// state is spelled out so that `blobs_stale_source_idx` and
/// `blobs_abandoned_idx` apply, see `state_literals`. Each kind takes up
/// to `limit` from its own index, as `expire_sql` says.
fn expire_abandoned_sql(limit: i64) -> String {
    let created = state_literals([BlobState::Created]);
    let sources = format!("state = {created} AND source_parts > 0 AND source_updated_at <= $2");
    let attached = format!(
        "state = {created} AND source_parts = 0 AND expires_at IS NULL AND updated_at <= $2"
    );
    format!(
        "UPDATE blobs SET state = $1{CLEAR_FAILURE} \
         WHERE id = ANY(ARRAY(\
             (SELECT id FROM blobs WHERE {sources} ORDER BY source_updated_at LIMIT {limit}) \
             UNION ALL \
             (SELECT id FROM blobs WHERE {attached} ORDER BY updated_at LIMIT {limit})\
         )) AND (({sources}) OR ({attached})) \
         RETURNING id"
    )
}

/// Terminal blobs whose objects are still in the bucket and that no purge
/// job, pending, running or dead, is going to take care of: `expired` and
/// `deleted` ones, and `infected` ones once past their `expires_at`, since
/// an infected file is kept for forensics but not forever. Objects means
/// the original or the source parts a generated blob held, see
/// `Blob::holds_objects`. The states are spelled into the SQL so that
/// `blobs_unpurged_idx` applies, see `state_literals`.
pub(crate) async fn awaiting_purge(
    conn: &mut PgConnection,
    now: Timestamp,
    limit: i64,
) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(AssertSqlSafe(awaiting_purge_sql(limit)))
        .bind(now.to_sqlx())
        .bind(JobKind::Purge.as_str())
        .fetch_all(&mut *conn)
        .await
}

/// `awaiting_purge`'s statement: `$1` the time, `$2` the purge's kind.
/// The three states are one list, the index's own, so that one walk of
/// `blobs_unpurged_idx` in id order serves the limit.
fn awaiting_purge_sql(limit: i64) -> String {
    format!(
        "SELECT id FROM blobs \
         WHERE (storage_key IS NOT NULL OR source_parts > 0) AND purged_at IS NULL \
           AND state IN ({}) AND (state <> {} OR expires_at <= $1) \
           AND NOT EXISTS (SELECT 1 FROM jobs WHERE jobs.kind = $2 AND jobs.blob_id = blobs.id) \
         ORDER BY id LIMIT {limit}",
        state_literals([BlobState::Infected, BlobState::Expired, BlobState::Deleted]),
        state_literals([BlobState::Infected])
    )
}

/// States as a list of SQL literals, `'expired', 'deleted'`, for a predicate
/// that has to match a partial index: the planner only uses one when it can
/// prove the query implies the index predicate, and it cannot prove
/// anything about a bound parameter. The names are the enum's own, so
/// nothing user-written reaches the SQL.
fn state_literals(states: impl IntoIterator<Item = BlobState>) -> String {
    states
        .into_iter()
        .map(|s| format!("'{}'", s.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The purge is done: nothing of the blob is left in the bucket.
pub(crate) async fn mark_purged(exec: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("UPDATE blobs SET purged_at = COALESCE(purged_at, now()) WHERE id = $1")
        .bind(id)
        .execute(exec)
        .await?;
    Ok(())
}

/// Remove the tombstones: terminal rows whose objects were purged before
/// `before`, or that went terminal before `before` with nothing to purge
/// (a blob created and never uploaded). Their attachments and jobs go with
/// them, and the id is then unknown like any other. An `infected` blob is
/// only ever pruned through its purge, which needs its expiry. At most
/// `limit` rows go per call, so a backlog drains over successive reaps
/// rather than in one transaction holding every row and its cascades. The
/// states are spelled into the SQL so that `blobs_empty_tombstone_idx`
/// applies, see `state_literals`.
pub(crate) async fn prune_tombstones(
    conn: &mut PgConnection,
    before: Timestamp,
    limit: i64,
) -> sqlx::Result<u64> {
    let result = sqlx::query(AssertSqlSafe(prune_tombstones_sql(limit)))
        .bind(before.to_sqlx())
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected())
}

/// `prune_tombstones`'s statement: `$1` the time. The limit is written
/// in, a constant, so that a generic plan knows it; and the delete takes
/// an array of ids rather than `IN (subquery)`, which such a plan may
/// answer with a scan of the whole table joined to the few ids. Each kind
/// of tombstone is walked on its own index, as `expire_sql` says.
fn prune_tombstones_sql(limit: i64) -> String {
    let purged = "purged_at <= $1";
    let empty = format!(
        "state IN ({}) AND purged_at IS NULL AND storage_key IS NULL \
         AND source_parts = 0 AND updated_at <= $1",
        state_literals([BlobState::Expired, BlobState::Deleted])
    );
    format!(
        "DELETE FROM blobs WHERE id = ANY(ARRAY(\
             SELECT id FROM (\
                 (SELECT id FROM blobs WHERE {purged} ORDER BY purged_at LIMIT {limit}) \
                 UNION ALL \
                 (SELECT id FROM blobs WHERE {empty} ORDER BY updated_at LIMIT {limit})\
             ) AS tombstones LIMIT {limit}\
         )) AND (({purged}) OR ({empty}))"
    )
}

/// Hand every upload without a sign of life since `before` back to
/// `created`, as `abort_upload` would have: nothing was stored. The one
/// rule for a failed upload, applied by the reaper to the ones a crashed
/// process left behind. A live upload touches its row as it runs, however
/// long it takes.
pub(crate) async fn reset_stale_uploads(
    conn: &mut PgConnection,
    before: Timestamp,
    limit: i64,
) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(AssertSqlSafe(reset_stale_uploads_sql(limit)))
        .bind(BlobState::Created.as_str())
        .bind(before.to_sqlx())
        .fetch_all(&mut *conn)
        .await
}

/// `reset_stale_uploads`'s statement: `$1` the state, `$2` the time. The
/// state is spelled out so that `blobs_uploading_idx` applies, see
/// `state_literals`; the batch is taken as `expire_sql` says, so an
/// upload that touched its row meanwhile is left alone.
fn reset_stale_uploads_sql(limit: i64) -> String {
    let stale = format!(
        "state = {} AND upload_heartbeat_at <= $2",
        state_literals([BlobState::Uploading])
    );
    format!(
        "UPDATE blobs SET state = $1 \
         WHERE id = ANY(ARRAY(\
             SELECT id FROM blobs WHERE {stale} ORDER BY upload_heartbeat_at LIMIT {limit}\
         )) AND {stale} \
         RETURNING id"
    )
}

/// The generic plans of the reaper's statements, by name, as `EXPLAIN`
/// prints them: each prepared and explained under `plan_cache_mode =
/// force_generic_plan`, which is the plan a pooled connection may settle
/// on after five runs, its parameters unknown. For a test that pins them
/// to their indexes.
#[cfg(any(test, feature = "test-support"))]
pub async fn explain_reaper(conn: &mut PgConnection) -> sqlx::Result<Vec<(&'static str, String)>> {
    const LIMIT: i64 = 10_000;
    let statements = [
        ("expire", expire_sql(LIMIT), "'expired', now()"),
        (
            "expire_abandoned",
            expire_abandoned_sql(LIMIT),
            "'expired', now()",
        ),
        (
            "awaiting_purge",
            awaiting_purge_sql(LIMIT),
            "now(), 'purge'",
        ),
        ("prune_tombstones", prune_tombstones_sql(LIMIT), "now()"),
        (
            "reset_stale_uploads",
            reset_stale_uploads_sql(LIMIT),
            "'created', now()",
        ),
    ];
    sqlx::query("SET plan_cache_mode = force_generic_plan")
        .execute(&mut *conn)
        .await?;
    let mut plans = Vec::new();
    for (name, sql, args) in statements {
        sqlx::raw_sql(AssertSqlSafe(format!("PREPARE reaper AS {sql}")))
            .execute(&mut *conn)
            .await?;
        let lines: Vec<String> =
            sqlx::query_scalar(AssertSqlSafe(format!("EXPLAIN EXECUTE reaper({args})")))
                .fetch_all(&mut *conn)
                .await?;
        sqlx::raw_sql("DEALLOCATE reaper")
            .execute(&mut *conn)
            .await?;
        plans.push((name, lines.join("\n")));
    }
    sqlx::query("RESET plan_cache_mode")
        .execute(&mut *conn)
        .await?;
    Ok(plans)
}

/// Record the outcome of rendering `kind` for `id`, along with the variant
/// version it was built with and, for `ready`, what was rendered. Blob
/// state is untouched: the representation is written even if the blob was
/// deleted meanwhile, and the purge takes it with the rest of the prefix.
pub(crate) async fn finish_representation(
    exec: impl PgExecutor<'_>,
    id: Uuid,
    kind: Representation,
    state: RepresentationState,
    source: Option<PreviewSource>,
    version: i32,
    rendered: Option<Rendered>,
) -> sqlx::Result<()> {
    let size = rendered
        .map(|r| i32::try_from(r.size))
        .transpose()
        .map_err(|e| sqlx::Error::Encode(e.into()))?;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("UPDATE blobs SET ");
    qb.push(kind.state_column())
        .push(" = ")
        .push_bind(state.as_str())
        .push(", ")
        .push(kind.version_column())
        .push(" = ")
        .push_bind(version)
        .push(", ")
        .push(kind.digest_column())
        .push(" = ")
        .push_bind(rendered.map(|r| r.sha256.to_vec()))
        .push(", ")
        .push(kind.size_column())
        .push(" = ")
        .push_bind(size);
    if kind == Representation::Preview {
        qb.push(", preview_source = ")
            .push_bind(source.map(PreviewSource::as_str));
    }
    qb.push(" WHERE id = ").push_bind(id);
    qb.build().execute(exec).await?;
    Ok(())
}

/// Ask for `kind` to be (re)generated: the representation goes back to
/// `pending` and a job is enqueued, if the blob is still `ready` and the
/// representation was not disabled at creation. The lazy path of the
/// `GET` handler, for a missing object or a stale variant version.
pub(crate) async fn request_representation(
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
        .push_bind(BlobState::Ready.as_str())
        .push(" AND ")
        .push(kind.state_column())
        .push(" <> ")
        .push_bind(RepresentationState::Disabled.as_str())
        .push(" RETURNING priority");
    let Some(rank) = qb
        .build_query_scalar::<i16>()
        .fetch_optional(&mut *tx)
        .await?
    else {
        tx.rollback().await?;
        return Ok(false);
    };
    let priority = Priority::try_from(rank).map_err(|e| sqlx::Error::Decode(e.into()))?;
    jobs::enqueue(
        &mut *tx,
        JobKind::Render,
        Some(id),
        priority,
        serde_json::json!({}),
        Timestamp::now(),
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}
