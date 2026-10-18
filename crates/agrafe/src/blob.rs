//! Blob model and state machine.

pub mod create;
pub mod read;
pub mod repo;
pub mod representation;
pub mod urls;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::app::Agrafe;
use crate::classify::Category;
use crate::jobs::Priority;
use crate::source::rows::{Column, Sheet};
use repo::{ListParams, Page};

/// Why a blob could not be deleted.
#[derive(Debug, thiserror::Error)]
pub enum DeleteError {
    /// Blobs are shared across attachments that do not know of each other,
    /// so a backend acting for one record cannot know the blob is free:
    /// detach instead, and a blob nobody uses expires on its own.
    #[error("blob is attached and cannot be deleted; detach it first")]
    Attached,
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

/// Why a listing was refused.
#[derive(Debug, thiserror::Error)]
pub enum ListError {
    #[error("limit must be between 1 and {0}")]
    Limit(usize),
    #[error("{0}")]
    ContentType(&'static str),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

impl Agrafe {
    /// A blob by id, in any state.
    pub async fn find_blob(&self, id: Uuid) -> sqlx::Result<Option<Blob>> {
        repo::find(self.pool(), id).await
    }

    /// List blobs, newest first. `content_type` is normalised here, and
    /// `limit` must lie in `1..=repo::MAX_LIMIT`.
    pub async fn list_blobs(&self, mut params: ListParams) -> Result<Page, ListError> {
        if let Some(raw) = &params.content_type {
            params.content_type =
                Some(create::normalize_content_type(raw).map_err(ListError::ContentType)?);
        }
        if !(1..=repo::MAX_LIMIT).contains(&params.limit) {
            return Err(ListError::Limit(repo::MAX_LIMIT));
        }
        Ok(repo::list(self.pool(), &params).await?)
    }

    /// Soft delete of an unattached blob: the objects are purged
    /// asynchronously. `false` when there is no such blob; deleting twice
    /// is not an error.
    pub async fn delete_blob(&self, id: Uuid) -> Result<bool, DeleteError> {
        let purge_at = self.config().upload.purge_at(Timestamp::now());
        match repo::soft_delete(self.pool(), id, purge_at).await? {
            repo::Delete::Done => Ok(true),
            repo::Delete::Attached => Err(DeleteError::Attached),
            repo::Delete::Missing => Ok(false),
        }
    }
}

/// Lifecycle of a blob. See DESIGN.md, "State machine".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobState {
    Created,
    Uploading,
    Uploaded,
    Ready,
    Infected,
    Expired,
    Deleted,
}

agrafe_types::str_enum!(BlobState {
    Created => "created",
    Uploading => "uploading",
    Uploaded => "uploaded",
    Ready => "ready",
    Infected => "infected",
    Expired => "expired",
    Deleted => "deleted",
});

impl BlobState {
    /// Terminal states never leave, except to `deleted`.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            BlobState::Infected | BlobState::Expired | BlobState::Deleted
        )
    }
}

/// Shape of the `PUT` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputType {
    Raw,
    Multipart,
    Rows,
    Archive,
}

agrafe_types::str_enum!(InputType {
    Raw => "raw",
    Multipart => "multipart",
    Rows => "rows",
    Archive => "archive",
});

impl InputType {
    /// `raw` and `multipart` carry the file itself and need a declared
    /// checksum and size. `rows` and `archive` are generated server side.
    pub fn is_direct(self) -> bool {
        matches!(self, InputType::Raw | InputType::Multipart)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanState {
    Pending,
    Clean,
    Infected,
    Skipped,
}

agrafe_types::str_enum!(ScanState {
    Pending => "pending",
    Clean => "clean",
    Infected => "infected",
    Skipped => "skipped",
});

/// How an upload ended with respect to scanning, which decides the state the
/// blob lands in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanOutcome {
    /// `skip_virus_scan`: the backend vouches for the bytes.
    Skipped,
    /// Scanned inline, nothing found.
    Clean,
    /// Scanned inline, the named signature matched.
    Infected(String),
    /// Too large to scan inline, or clamd was unreachable: a job will.
    Deferred,
}

impl ScanOutcome {
    pub fn states(&self) -> (BlobState, ScanState) {
        match self {
            ScanOutcome::Skipped => (BlobState::Ready, ScanState::Skipped),
            ScanOutcome::Clean => (BlobState::Ready, ScanState::Clean),
            ScanOutcome::Infected(_) => (BlobState::Infected, ScanState::Infected),
            ScanOutcome::Deferred => (BlobState::Uploaded, ScanState::Pending),
        }
    }

    pub fn is_scanned(&self) -> bool {
        matches!(self, ScanOutcome::Clean | ScanOutcome::Infected(_))
    }

    pub fn signature(&self) -> Option<&str> {
        match self {
            ScanOutcome::Infected(name) => Some(name),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepresentationState {
    Pending,
    Ready,
    Failed,
    Unavailable,
}

agrafe_types::str_enum!(RepresentationState {
    Pending => "pending",
    Ready => "ready",
    Failed => "failed",
    Unavailable => "unavailable",
});

/// The two derived representations of a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Representation {
    Thumbnail,
    Preview,
}

agrafe_types::str_enum!(Representation {
    Thumbnail => "thumbnail",
    Preview => "preview",
});

impl Representation {
    /// Object key of the representation, under the blob's prefix.
    pub fn object_key(self, blob_id: Uuid) -> String {
        format!("{blob_id}/{self}")
    }

    /// The `blobs` column holding its state. Code-owned names, safe to
    /// splice into SQL.
    pub fn state_column(self) -> &'static str {
        match self {
            Representation::Thumbnail => "thumbnail_state",
            Representation::Preview => "preview_state",
        }
    }

    pub fn version_column(self) -> &'static str {
        match self {
            Representation::Thumbnail => "thumbnail_variant_version",
            Representation::Preview => "preview_variant_version",
        }
    }
}

/// What a preview was rendered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewSource {
    Image,
    EmbeddedImage,
    FirstPage,
}

agrafe_types::str_enum!(PreviewSource {
    Image => "image",
    EmbeddedImage => "embedded_image",
    FirstPage => "first_page",
});

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Blob {
    pub id: Uuid,
    #[sqlx(try_from = "String")]
    pub state: BlobState,
    pub filename: String,
    pub content_type: String,
    #[sqlx(try_from = "String")]
    pub category: Category,
    #[sqlx(try_from = "String")]
    pub input_type: InputType,
    pub checksum_sha256: Option<Vec<u8>>,
    pub size: Option<i64>,
    pub skip_virus_scan: bool,
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub expires_at: Option<Timestamp>,
    /// Copied onto every job enqueued for the blob. Stored as its rank,
    /// like on `jobs`.
    #[sqlx(try_from = "i16")]
    pub priority: Priority,

    #[sqlx(try_from = "String")]
    pub scan_state: ScanState,
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub scanned_at: Option<Timestamp>,
    /// What clamd found, when the blob is `infected`.
    pub scan_signature: Option<String>,

    /// Object key of the original, `<id>/original`.
    pub storage_key: Option<String>,

    /// Source parts accepted so far, for a `rows` or `archive` blob; the
    /// next one must be `source_parts + 1`. Always zero for a direct blob.
    pub source_parts: i32,
    /// Lines across the parts: manifest entries for an archive.
    pub source_rows: i64,
    /// Bytes of source across the parts.
    pub source_size: i64,
    /// When the last part landed.
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub source_updated_at: Option<Timestamp>,
    /// The sheets of a `rows` blob, one for now, each with its columns in
    /// order: declared at creation, read by accept and build.
    pub source_columns: Option<sqlx::types::Json<Vec<Sheet>>>,
    /// Set by the archive build: members written, and members skipped
    /// because they were gone by then.
    pub archive_entries: Option<i64>,
    pub archive_errors: Option<i64>,

    pub kek_id: Option<String>,
    pub wrapped_key: Option<Vec<u8>>,
    /// Streaming AEAD header (salt and nonce prefix), cached from the object.
    pub encryption_header: Option<Vec<u8>>,
    pub chunk_size: Option<i32>,

    #[sqlx(try_from = "String")]
    pub thumbnail_state: RepresentationState,
    #[sqlx(try_from = "String")]
    pub preview_state: RepresentationState,
    pub preview_source: Option<String>,
    pub thumbnail_variant_version: Option<i32>,
    pub preview_variant_version: Option<i32>,

    #[sqlx(try_from = "jiff_sqlx::Timestamp")]
    pub created_at: Timestamp,
    #[sqlx(try_from = "jiff_sqlx::Timestamp")]
    pub updated_at: Timestamp,
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub upload_started_at: Option<Timestamp>,
    /// Last sign of life from the process running the upload.
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub upload_heartbeat_at: Option<Timestamp>,
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub uploaded_at: Option<Timestamp>,
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub deleted_at: Option<Timestamp>,
    /// When the purge removed the objects from the bucket.
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub purged_at: Option<Timestamp>,
}

impl Blob {
    /// The single rule deciding whether the public port may upload this blob.
    /// Enforced when minting and again in the public `PUT` handler.
    pub fn public_upload_allowed(&self) -> bool {
        public_upload_allowed(self.skip_virus_scan, self.input_type)
    }

    /// The state the blob is in at `now`. A blob past `expires_at` is
    /// `expired` from that instant, whether or not the reaper has recorded it
    /// yet, so expiry never waits on the reaper's schedule.
    pub fn state_at(&self, now: Timestamp) -> BlobState {
        match self.expires_at {
            Some(at) if at <= now && !self.state.is_terminal() => BlobState::Expired,
            _ => self.state,
        }
    }

    /// The public port answers `404` for blobs in a terminal state, so a leaked
    /// URL reveals nothing once the blob is gone. The private port still sees
    /// them.
    pub fn visible_on_public(&self, now: Timestamp) -> bool {
        !self.state_at(now).is_terminal()
    }

    /// Representations may be served only from `ready`.
    pub fn is_readable(&self, now: Timestamp) -> bool {
        self.state_at(now) == BlobState::Ready
    }

    pub fn representation_state(&self, kind: Representation) -> RepresentationState {
        match kind {
            Representation::Thumbnail => self.thumbnail_state,
            Representation::Preview => self.preview_state,
        }
    }

    pub fn representation_version(&self, kind: Representation) -> Option<i32> {
        match kind {
            Representation::Thumbnail => self.thumbnail_variant_version,
            Representation::Preview => self.preview_variant_version,
        }
    }

    /// Blobs are write-once: a `PUT` may start only before any bytes exist.
    pub fn accepts_upload(&self, now: Timestamp) -> bool {
        self.state_at(now) == BlobState::Created
    }

    /// Whether something of the blob is in the bucket: the original, or the
    /// source parts a generated blob holds while it is still `created`. A
    /// delete or an expiry of such a blob needs a purge.
    pub fn holds_objects(&self) -> bool {
        self.storage_key.is_some() || self.source_parts > 0
    }

    /// Object key of source part `n` of a generated blob.
    pub fn source_key(id: Uuid, n: u32) -> String {
        format!("{id}/source/{n}")
    }

    /// The one sheet of a `rows` blob.
    pub fn sheet(&self) -> Option<&Sheet> {
        self.source_columns
            .as_ref()
            .and_then(|sheets| sheets.0.first())
    }

    /// The columns of a `rows` blob, in sheet order.
    pub fn columns(&self) -> Option<&[Column]> {
        self.sheet().map(|sheet| sheet.columns.as_slice())
    }
}

pub fn public_upload_allowed(skip_virus_scan: bool, input_type: InputType) -> bool {
    !skip_virus_scan && input_type.is_direct()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_upload_rule() {
        assert!(public_upload_allowed(false, InputType::Raw));
        assert!(public_upload_allowed(false, InputType::Multipart));
        assert!(!public_upload_allowed(true, InputType::Raw));
        assert!(!public_upload_allowed(false, InputType::Rows));
        assert!(!public_upload_allowed(false, InputType::Archive));
    }

    #[test]
    fn enums_round_trip() {
        for s in BlobState::ALL {
            assert_eq!(s.as_str().parse::<BlobState>().unwrap(), *s);
        }
        assert!("bogus".parse::<BlobState>().is_err());
        assert_eq!(serde_json::to_string(&InputType::Raw).unwrap(), "\"raw\"");
    }
}
