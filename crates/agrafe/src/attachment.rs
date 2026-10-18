//! Attachments: a record of the backend uses a blob. See DESIGN.md,
//! "Attachments".
//!
//! An attachment is three strings, `record_type`, `record_id` and `name`,
//! against a blob. A blob with at least one attachment carries no expiry:
//! the first attach clears `expires_at`, the last detach sets it to the
//! default again. Attach and detach lock the blob row for their
//! transaction, so the expiry and the attachments change together, and
//! the reaper's expiry update waits on the same lock.

use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use serde::Deserialize;
use sqlx::{PgConnection, PgExecutor, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::app::Agrafe;
use crate::blob::{Blob, BlobState};
use crate::text::is_bidi_control;

/// Why a blob could not be attached.
#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    #[error("no such blob")]
    NotFound,
    /// The request is well formed but not acceptable; the message says why.
    #[error("{0}")]
    Invalid(String),
    /// The blob is in a terminal state, or past its expiry whether or not
    /// the reaper has recorded it.
    #[error("blob is {0} and cannot be attached")]
    Terminal(BlobState),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

/// Why an attachment listing was refused.
#[derive(Debug, thiserror::Error)]
pub enum ListError {
    #[error("limit must be between 1 and {0}")]
    Limit(usize),
    #[error("record_id is only accepted together with record_type")]
    RecordIdAlone,
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

impl Agrafe {
    /// Attach `blob_id` to a record. Idempotent: the triple already
    /// attached hands back the existing attachment. Refused for a blob in a
    /// terminal state or past its expiry.
    pub async fn attach_blob(
        &self,
        blob_id: Uuid,
        req: AttachmentRequest,
    ) -> Result<Attached, AttachError> {
        let new = validate(req)?;
        let mut tx = self.pool().begin().await?;
        let blob = lock_blob(&mut tx, blob_id)
            .await?
            .ok_or(AttachError::NotFound)?;
        let state = blob.state_at(Timestamp::now());
        if state.is_terminal() {
            return Err(AttachError::Terminal(state));
        }
        let attached = insert(&mut tx, blob_id, &new).await?;
        clear_expiry(&mut tx, blob_id).await?;
        tx.commit().await?;
        Ok(attached)
    }

    /// Remove an attachment. `false` when there is no such attachment.
    /// Works on a blob in any state; the last detach re-arms the expiry
    /// even on a terminal blob, which gives an infected one its purge
    /// clock once the backend lets go of it.
    pub async fn detach_blob(&self, id: Uuid) -> sqlx::Result<bool> {
        let mut tx = self.pool().begin().await?;
        let Some(attachment) = find(&mut *tx, id).await? else {
            return Ok(false);
        };
        // The blob row goes first, as on attach, so the two never wait on
        // each other in opposite orders.
        if lock_blob(&mut tx, attachment.blob_id).await?.is_none() {
            // The blob was pruned meanwhile, and the attachment with it.
            return Ok(false);
        }
        if !delete(&mut *tx, id).await? {
            // Another detach took it between `find` and the lock.
            return Ok(false);
        }
        if count_for_blob(&mut *tx, attachment.blob_id).await? == 0 {
            let expires_at = self.config().blobs.default_expires_at(Timestamp::now());
            set_expiry(&mut tx, attachment.blob_id, expires_at).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn find_attachment(&self, id: Uuid) -> sqlx::Result<Option<Attachment>> {
        find(self.pool(), id).await
    }

    /// A blob's attachments, oldest first.
    pub async fn attachments_of(&self, blob_id: Uuid) -> sqlx::Result<Vec<Attachment>> {
        for_blobs(self.pool(), &[blob_id]).await
    }

    /// The attachments of every blob among `ids`, oldest first.
    pub async fn attachments_for(&self, ids: &[Uuid]) -> sqlx::Result<Vec<Attachment>> {
        for_blobs(self.pool(), ids).await
    }

    /// List attachments, newest first. `limit` must lie in
    /// `1..=MAX_LIMIT`, and `record_id` needs `record_type`.
    pub async fn list_attachments(&self, params: &ListParams) -> Result<Page, ListError> {
        if !(1..=MAX_LIMIT).contains(&params.limit) {
            return Err(ListError::Limit(MAX_LIMIT));
        }
        if params.record_id.is_some() && params.record_type.is_none() {
            return Err(ListError::RecordIdAlone);
        }
        Ok(list(self.pool(), params).await?)
    }
}

/// What `POST /blobs/:id/attachments` takes, and `POST /blobs` under
/// `attachment`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentRequest {
    pub record_type: String,
    pub record_id: String,
    pub name: String,
}

/// A validated request, ready to insert.
#[derive(Debug, Clone)]
pub struct NewAttachment {
    pub record_type: String,
    pub record_id: String,
    pub name: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Attachment {
    pub id: Uuid,
    pub blob_id: Uuid,
    pub record_type: String,
    pub record_id: String,
    pub name: String,
    #[sqlx(try_from = "jiff_sqlx::Timestamp")]
    pub created_at: Timestamp,
}

/// What an attach handed back: the attachment it created, or the one
/// that already existed for the triple.
#[derive(Debug, Clone)]
pub enum Attached {
    Created(Attachment),
    Existing(Attachment),
}

impl Attached {
    pub fn attachment(&self) -> &Attachment {
        match self {
            Attached::Created(a) | Attached::Existing(a) => a,
        }
    }
}

/// An attachment as the listing shows it: with the state of its blob, so
/// that a record's side can tell a live file from an expired or infected
/// one without a second request.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Listed {
    #[sqlx(flatten)]
    pub attachment: Attachment,
    #[sqlx(try_from = "String")]
    pub blob_state: BlobState,
}

#[derive(Debug, Default, Clone)]
pub struct ListParams {
    pub record_type: Option<String>,
    /// Only together with `record_type`.
    pub record_id: Option<String>,
    pub name: Option<String>,
    pub blob_id: Option<Uuid>,
    pub cursor: Option<Uuid>,
    /// Already validated to lie in `1..=MAX_LIMIT`.
    pub limit: usize,
}

pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 200;

pub struct Page {
    pub items: Vec<Listed>,
    pub next_cursor: Option<Uuid>,
}

/// The three strings have the rules of a filename, short of its path
/// separators: an identifier is the backend's to write.
pub const MAX_FIELD_LEN: usize = 255;

pub fn validate(req: AttachmentRequest) -> Result<NewAttachment, AttachError> {
    validate_field("record_type", &req.record_type)?;
    validate_field("record_id", &req.record_id)?;
    validate_field("name", &req.name)?;
    Ok(NewAttachment {
        record_type: req.record_type,
        record_id: req.record_id,
        name: req.name,
    })
}

fn validate_field(field: &str, value: &str) -> Result<(), AttachError> {
    if value.is_empty() {
        return Err(AttachError::Invalid(format!("{field} must not be empty")));
    }
    if value.len() > MAX_FIELD_LEN {
        return Err(AttachError::Invalid(format!(
            "{field} is too long ({MAX_FIELD_LEN} bytes at most)"
        )));
    }
    if value.chars().any(|c| c.is_control() || is_bidi_control(c)) {
        return Err(AttachError::Invalid(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

/// The blob row, locked for the rest of the transaction. The reaper's
/// expiry update and a delete wait on it, so what is read here holds
/// until the commit.
async fn lock_blob(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Option<Blob>> {
    sqlx::query_as::<_, Blob>("SELECT * FROM blobs WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(conn)
        .await
}

/// Insert, or find the attachment the unique index already holds.
async fn insert(
    conn: &mut PgConnection,
    blob_id: Uuid,
    new: &NewAttachment,
) -> sqlx::Result<Attached> {
    let inserted = sqlx::query_as::<_, Attachment>(
        "INSERT INTO attachments (id, blob_id, record_type, record_id, name) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (record_type, record_id, name, blob_id) DO NOTHING \
         RETURNING *",
    )
    .bind(Uuid::now_v7())
    .bind(blob_id)
    .bind(&new.record_type)
    .bind(&new.record_id)
    .bind(&new.name)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(attachment) = inserted {
        return Ok(Attached::Created(attachment));
    }
    let existing = sqlx::query_as::<_, Attachment>(
        "SELECT * FROM attachments \
         WHERE record_type = $1 AND record_id = $2 AND name = $3 AND blob_id = $4",
    )
    .bind(&new.record_type)
    .bind(&new.record_id)
    .bind(&new.name)
    .bind(blob_id)
    .fetch_one(conn)
    .await?;
    Ok(Attached::Existing(existing))
}

/// The attachment a blob is created with, in the creating transaction.
/// The blob was just inserted without an expiry, so there is nothing to
/// clear.
pub async fn insert_with_blob(
    conn: &mut PgConnection,
    blob_id: Uuid,
    new: &NewAttachment,
) -> sqlx::Result<Attachment> {
    match insert(conn, blob_id, new).await? {
        Attached::Created(a) | Attached::Existing(a) => Ok(a),
    }
}

/// The blob is attached: it no longer expires.
async fn clear_expiry(conn: &mut PgConnection, blob_id: Uuid) -> sqlx::Result<()> {
    sqlx::query("UPDATE blobs SET expires_at = NULL WHERE id = $1 AND expires_at IS NOT NULL")
        .bind(blob_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// The blob lost its last attachment: it expires again, at `expires_at`.
async fn set_expiry(
    conn: &mut PgConnection,
    blob_id: Uuid,
    expires_at: Timestamp,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE blobs SET expires_at = $2 WHERE id = $1")
        .bind(blob_id)
        .bind(expires_at.to_sqlx())
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn find(exec: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<Option<Attachment>> {
    sqlx::query_as::<_, Attachment>("SELECT * FROM attachments WHERE id = $1")
        .bind(id)
        .fetch_optional(exec)
        .await
}

async fn delete(exec: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<bool> {
    let result = sqlx::query("DELETE FROM attachments WHERE id = $1")
        .bind(id)
        .execute(exec)
        .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn count_for_blob(exec: impl PgExecutor<'_>, blob_id: Uuid) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT count(*) FROM attachments WHERE blob_id = $1")
        .bind(blob_id)
        .fetch_one(exec)
        .await
}

/// The attachments of every blob among `ids`, oldest first.
pub async fn for_blobs(exec: impl PgExecutor<'_>, ids: &[Uuid]) -> sqlx::Result<Vec<Attachment>> {
    sqlx::query_as::<_, Attachment>("SELECT * FROM attachments WHERE blob_id = ANY($1) ORDER BY id")
        .bind(ids)
        .fetch_all(exec)
        .await
}

/// Newest first, keyset paginated on the UUID v7 id, each row with its
/// blob's state.
pub async fn list(exec: impl PgExecutor<'_>, params: &ListParams) -> sqlx::Result<Page> {
    let limit = params.limit;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT a.*, b.state AS blob_state FROM attachments a \
         JOIN blobs b ON b.id = a.blob_id WHERE true",
    );
    if let Some(record_type) = &params.record_type {
        qb.push(" AND a.record_type = ").push_bind(record_type);
    }
    if let Some(record_id) = &params.record_id {
        qb.push(" AND a.record_id = ").push_bind(record_id);
    }
    if let Some(name) = &params.name {
        qb.push(" AND a.name = ").push_bind(name);
    }
    if let Some(blob_id) = params.blob_id {
        qb.push(" AND a.blob_id = ").push_bind(blob_id);
    }
    if let Some(cursor) = params.cursor {
        qb.push(" AND a.id < ").push_bind(cursor);
    }
    qb.push(" ORDER BY a.id DESC LIMIT ")
        .push_bind(limit as i64 + 1);

    let mut items: Vec<Listed> = qb.build_query_as().fetch_all(exec).await?;
    let next_cursor = if items.len() > limit {
        items.truncate(limit);
        items.last().map(|a| a.attachment.id)
    } else {
        None
    };
    Ok(Page { items, next_cursor })
}

/// Whether any attachment references `blob_id`. The caller holds the blob
/// row lock when the answer decides something.
pub async fn exists_for_blob(exec: impl PgExecutor<'_>, blob_id: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM attachments WHERE blob_id = $1)")
        .bind(blob_id)
        .fetch_one(exec)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(record_type: &str, record_id: &str, name: &str) -> AttachmentRequest {
        AttachmentRequest {
            record_type: record_type.into(),
            record_id: record_id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn fields_are_validated() {
        assert!(validate(req("Dossier", "42", "piece_justificative")).is_ok());
        assert!(
            validate(req("User", "a b/c:d", "avatar")).is_ok(),
            "ids are the backend's"
        );
        assert!(validate(req("صورة", "١", "x")).is_ok());
        for (t, i, n) in [
            ("", "1", "x"),
            ("T", "", "x"),
            ("T", "1", ""),
            ("T\n", "1", "x"),
            ("T", "1\u{0}", "x"),
            ("T", "1", "x\u{202E}"),
            ("T", "1", "\t"),
        ] {
            let err = validate(req(t, i, n)).unwrap_err();
            assert!(matches!(err, AttachError::Invalid(_)), "{t:?} {i:?} {n:?}");
        }
        let long = "x".repeat(MAX_FIELD_LEN);
        assert!(validate(req(&long, &long, &long)).is_ok());
        let too_long = "x".repeat(MAX_FIELD_LEN + 1);
        assert!(validate(req("T", &too_long, "x")).is_err());
    }
}
