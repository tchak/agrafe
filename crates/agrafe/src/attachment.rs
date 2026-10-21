//! Attachments: an owner in the backend uses a blob. See [DESIGN.md,
//! "Attachments"].
//!
//! An attachment is three strings, `owner_kind`, `owner_id` and `role`,
//! against a blob. A blob with at least one attachment carries no expiry:
//! the first attach clears `expires_at`, the last detach sets it to the
//! default again. Attach and detach lock the blob row for their
//! transaction, so the expiry and the attachments change together, and
//! the reaper's expiry update waits on the same lock.
//!
//! [DESIGN.md, "Attachments"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#attachments

use std::collections::BTreeSet;

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
    /// No blob has this id.
    #[error("no such blob")]
    NotFound,
    /// The request is well formed but not acceptable; the message says why.
    #[error("{0}")]
    Invalid(String),
    /// The blob is in a terminal state, or past its expiry whether or not
    /// the reaper has recorded it.
    #[error("blob is {0} and cannot be attached")]
    Terminal(BlobState),
    /// The query or the transaction failed.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

/// Why an attachment listing was refused.
#[derive(Debug, thiserror::Error)]
pub enum ListError {
    /// `limit` lies outside `1..=MAX_LIMIT`; carries `MAX_LIMIT`.
    #[error("limit must be between 1 and {0}")]
    Limit(usize),
    /// `owner_id` was given without `owner_kind`, which it means nothing
    /// without.
    #[error("owner_id is only accepted together with owner_kind")]
    OwnerIdAlone,
    /// The query failed.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

impl Agrafe {
    /// Attach `blob_id` to an owner. Idempotent: the triple already
    /// attached hands back the existing attachment. Refused for a blob in a
    /// terminal state or past its expiry.
    pub async fn attach_blob(
        &self,
        blob_id: Uuid,
        req: AttachmentRequest,
    ) -> Result<Attached, AttachError> {
        let new = validate(req).map_err(AttachError::Invalid)?;
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
        if !exists_for_blob(&mut *tx, attachment.blob_id).await? {
            let expires_at = self.config().blobs.default_expires_at(Timestamp::now());
            set_expiry(&mut tx, attachment.blob_id, expires_at).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// The attachment with this id, or `None`.
    pub async fn find_attachment(&self, id: Uuid) -> sqlx::Result<Option<Attachment>> {
        find(self.pool(), id).await
    }

    /// The attachments of every blob among `ids`, oldest first.
    pub async fn attachments_for(&self, ids: &[Uuid]) -> sqlx::Result<Vec<Attachment>> {
        for_blobs(self.pool(), ids).await
    }

    /// The distinct `owner_kind` and `role` values in the table, sorted,
    /// so a filter can offer them as a list. A backend has a handful of
    /// owner kinds and a handful of roles per kind, but the table can
    /// hold a billion rows, and `SELECT DISTINCT` reads all of them: there
    /// is no loose index scan in Postgres. So the distinct `(owner_kind,
    /// role)` pairs are walked by hand along `attachments_kind_role_idx`,
    /// one index probe per pair, in a recursive CTE, and both facets come
    /// from the pairs; at most `MAX_FACETS` pairs are walked.
    pub async fn attachment_facets(&self) -> sqlx::Result<Facets> {
        let pairs: Vec<(String, String)> = sqlx::query_as(
            "WITH RECURSIVE pairs AS ( \
                 (SELECT owner_kind, role FROM attachments \
                  ORDER BY owner_kind, role LIMIT 1) \
                 UNION ALL \
                 SELECT next.owner_kind, next.role \
                 FROM pairs, LATERAL ( \
                     SELECT owner_kind, role FROM attachments \
                     WHERE (owner_kind, role) > (pairs.owner_kind, pairs.role) \
                     ORDER BY owner_kind, role LIMIT 1 \
                 ) next \
             ) \
             SELECT owner_kind, role FROM pairs LIMIT $1",
        )
        .bind(MAX_FACETS as i64)
        .fetch_all(self.pool())
        .await?;
        let mut owner_kinds = Vec::new();
        let mut roles = BTreeSet::new();
        for (owner_kind, role) in pairs {
            if owner_kinds.last() != Some(&owner_kind) {
                owner_kinds.push(owner_kind);
            }
            roles.insert(role);
        }
        Ok(Facets {
            owner_kinds,
            roles: roles.into_iter().collect(),
        })
    }

    /// List attachments, newest first. `limit` must lie in
    /// `1..=MAX_LIMIT`, and `owner_id` needs `owner_kind`.
    pub async fn list_attachments(&self, params: &ListParams) -> Result<Page, ListError> {
        if !(1..=MAX_LIMIT).contains(&params.limit) {
            return Err(ListError::Limit(MAX_LIMIT));
        }
        if params.owner_id.is_some() && params.owner_kind.is_none() {
            return Err(ListError::OwnerIdAlone);
        }
        Ok(list(self.pool(), params).await?)
    }
}

/// What `POST /blobs/:id/attachments` takes, and `POST /blobs` under
/// `attachment`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentRequest {
    /// The kind of owner, as the backend names it.
    pub owner_kind: String,
    /// The owner's identifier, as the backend writes it.
    pub owner_id: String,
    /// The role the blob plays for the owner (`avatar`).
    pub role: String,
}

/// A validated request, ready to insert.
#[derive(Debug, Clone)]
pub(crate) struct NewAttachment {
    pub owner_kind: String,
    pub owner_id: String,
    pub role: String,
}

/// A row of `attachments`: an owner in the backend uses a blob.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Attachment {
    /// A UUIDv7, so the listing's order and cursor.
    pub id: Uuid,
    /// The blob in use.
    pub blob_id: Uuid,
    /// The kind of owner.
    pub owner_kind: String,
    /// The owner's identifier, as the backend writes it.
    pub owner_id: String,
    /// The role the blob plays for the owner.
    pub role: String,
    /// When the attachment was made.
    #[sqlx(try_from = "jiff_sqlx::Timestamp")]
    pub created_at: Timestamp,
}

/// What an attach handed back: the attachment it created, or the one
/// that already existed for the triple.
#[derive(Debug, Clone)]
pub enum Attached {
    /// The triple was new.
    Created(Attachment),
    /// The triple was already attached to the blob.
    Existing(Attachment),
}

impl Attached {
    /// The attachment, created or existing.
    pub fn attachment(&self) -> &Attachment {
        match self {
            Attached::Created(a) | Attached::Existing(a) => a,
        }
    }
}

/// An attachment as the listing shows it: with the state of its blob, and
/// whether the job that was to finish the blob died, so that an owner's
/// side can tell a live file from an expired, infected or stuck one
/// without a second request.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Listed {
    /// The attachment itself.
    #[sqlx(flatten)]
    pub attachment: Attachment,
    /// The state of its blob.
    #[sqlx(try_from = "String")]
    pub blob_state: BlobState,
    /// The blob's `failed_at`: set when the job that was to finish it ran
    /// out of attempts.
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub blob_failed_at: Option<Timestamp>,
}

/// The filters and page of an attachment listing; every filter given must
/// match.
#[derive(Debug, Default, Clone)]
pub struct ListParams {
    /// Only attachments of this kind of owner.
    pub owner_kind: Option<String>,
    /// Only together with `owner_kind`.
    pub owner_id: Option<String>,
    /// Only attachments in this role.
    pub role: Option<String>,
    /// Only the attachments of this blob.
    pub blob_id: Option<Uuid>,
    /// The `next_cursor` of the previous page: only attachments older than
    /// it.
    pub cursor: Option<Uuid>,
    /// The page size, in `1..=MAX_LIMIT`.
    pub limit: usize,
}

/// The page size when the request gives none.
pub const DEFAULT_LIMIT: usize = 50;
/// The largest page a listing hands back.
pub const MAX_LIMIT: usize = 200;

/// The values `owner_kind` and `role` take across the table; see
/// `Agrafe::attachment_facets`.
#[derive(Debug, Clone, Default)]
pub struct Facets {
    /// The distinct `owner_kind` values, sorted.
    pub owner_kinds: Vec<String>,
    /// The distinct `role` values across every owner kind, sorted.
    pub roles: Vec<String>,
}

/// Distinct `(owner_kind, role)` pairs walked at most for the facets.
pub const MAX_FACETS: usize = 500;

/// A page of an attachment listing.
pub struct Page {
    /// The attachments, newest first.
    pub items: Vec<Listed>,
    /// The cursor of the next page; `None` on the last one.
    pub next_cursor: Option<Uuid>,
}

/// The three strings have the rules of a filename, short of its path
/// separators: an identifier is the backend's to write, kept as written,
/// so nothing is trimmed, but blank is not an identifier.
pub const MAX_FIELD_LEN: usize = 255;

/// The error is the message; the callers wrap it in their own error.
pub(crate) fn validate(req: AttachmentRequest) -> Result<NewAttachment, String> {
    validate_field("owner_kind", &req.owner_kind)?;
    validate_field("owner_id", &req.owner_id)?;
    validate_field("role", &req.role)?;
    Ok(NewAttachment {
        owner_kind: req.owner_kind,
        owner_id: req.owner_id,
        role: req.role,
    })
}

fn validate_field(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.len() > MAX_FIELD_LEN {
        return Err(format!(
            "{field} is too long ({MAX_FIELD_LEN} bytes at most)"
        ));
    }
    if value.chars().any(|c| c.is_control() || is_bidi_control(c)) {
        return Err(format!("{field} must not contain control characters"));
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
        "INSERT INTO attachments (id, blob_id, owner_kind, owner_id, role) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (owner_kind, owner_id, role, blob_id) DO NOTHING \
         RETURNING *",
    )
    .bind(Uuid::now_v7())
    .bind(blob_id)
    .bind(&new.owner_kind)
    .bind(&new.owner_id)
    .bind(&new.role)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(attachment) = inserted {
        return Ok(Attached::Created(attachment));
    }
    let existing = sqlx::query_as::<_, Attachment>(
        "SELECT * FROM attachments \
         WHERE owner_kind = $1 AND owner_id = $2 AND role = $3 AND blob_id = $4",
    )
    .bind(&new.owner_kind)
    .bind(&new.owner_id)
    .bind(&new.role)
    .bind(blob_id)
    .fetch_one(conn)
    .await?;
    Ok(Attached::Existing(existing))
}

/// The attachment a blob is created with, in the creating transaction.
/// The blob was just inserted without an expiry, so there is nothing to
/// clear.
pub(crate) async fn insert_with_blob(
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

pub(crate) async fn find(exec: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<Option<Attachment>> {
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

/// The attachments of every blob among `ids`, oldest first.
pub(crate) async fn for_blobs(
    exec: impl PgExecutor<'_>,
    ids: &[Uuid],
) -> sqlx::Result<Vec<Attachment>> {
    sqlx::query_as::<_, Attachment>("SELECT * FROM attachments WHERE blob_id = ANY($1) ORDER BY id")
        .bind(ids)
        .fetch_all(exec)
        .await
}

/// Newest first, keyset paginated on the UUID v7 id, each row with its
/// blob's state.
pub(crate) async fn list(exec: impl PgExecutor<'_>, params: &ListParams) -> sqlx::Result<Page> {
    let limit = params.limit;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT a.*, b.state AS blob_state, b.failed_at AS blob_failed_at FROM attachments a \
         JOIN blobs b ON b.id = a.blob_id WHERE true",
    );
    if let Some(owner_kind) = &params.owner_kind {
        qb.push(" AND a.owner_kind = ").push_bind(owner_kind);
    }
    if let Some(owner_id) = &params.owner_id {
        qb.push(" AND a.owner_id = ").push_bind(owner_id);
    }
    if let Some(role) = &params.role {
        qb.push(" AND a.role = ").push_bind(role);
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

/// Whether any attachment references `blob_id`: the last-detach check and
/// delete's refusal. The caller holds the blob row lock when the answer
/// decides something.
pub(crate) async fn exists_for_blob(
    exec: impl PgExecutor<'_>,
    blob_id: Uuid,
) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM attachments WHERE blob_id = $1)")
        .bind(blob_id)
        .fetch_one(exec)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(owner_kind: &str, owner_id: &str, role: &str) -> AttachmentRequest {
        AttachmentRequest {
            owner_kind: owner_kind.into(),
            owner_id: owner_id.into(),
            role: role.into(),
        }
    }

    #[test]
    fn fields_are_validated() {
        assert!(validate(req("expense", "42", "receipt")).is_ok());
        assert!(
            validate(req("user", "a b/c:d", "avatar")).is_ok(),
            "ids are the backend's"
        );
        assert!(validate(req("صورة", "١", "x")).is_ok());
        for (t, i, n) in [
            ("", "1", "x"),
            ("T", "", "x"),
            ("T", "1", ""),
            ("T", "  ", "x"),
            ("T\n", "1", "x"),
            ("T", "1\u{0}", "x"),
            ("T", "1", "x\u{202E}"),
            ("T", "1", "\t"),
        ] {
            assert!(validate(req(t, i, n)).is_err(), "{t:?} {i:?} {n:?}");
        }
        let long = "x".repeat(MAX_FIELD_LEN);
        assert!(validate(req(&long, &long, &long)).is_ok());
        let too_long = "x".repeat(MAX_FIELD_LEN + 1);
        assert!(validate(req("T", &too_long, "x")).is_err());
    }
}
