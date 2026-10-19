//! Blob model and state machine.

pub mod cache;
pub mod create;
pub mod envelope;
pub mod metadata;
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
use crate::source::rows::Sheet;
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

/// Why nothing was enqueued for a blob.
#[derive(Debug, thiserror::Error)]
pub enum RetryError {
    #[error("blob not found")]
    NotFound,
    /// Expired, deleted or infected: nothing is going to finish it.
    #[error("blob is {0}")]
    Terminal(BlobState),
    /// The blob is not waiting on a job: `created` and not fetched, or
    /// `ready` with its representations settled.
    #[error("nothing to retry: a {0} blob waits on no job")]
    Nothing(BlobState),
    /// The job it waits on is queued or running already.
    #[error("a job is already pending for this blob")]
    Pending,
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

    /// What serving a blob needs of its row, in any state. The read routes
    /// are the hot path, and this is the one query on it, answered from
    /// the cache for a settled row served lately; see `blob::cache`.
    pub async fn find_blob_head(&self, id: Uuid) -> sqlx::Result<Option<BlobHead>> {
        if let Some(entry) = self.heads().get(id) {
            self.metrics().head_cache(true);
            return Ok(entry);
        }
        let entry = repo::find_head(self.pool(), id).await?;
        if self.heads().enabled() {
            self.metrics().head_cache(false);
            self.heads().insert(id, &entry);
        }
        Ok(entry)
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

    /// Enqueue whatever the blob is waiting on, the fetch, scan, build or render
    /// its state calls for, superseding a dead job of that kind and
    /// clearing the failure recorded on the row. The repair for a stuck
    /// blob, from the blob's side; works after the dead job is pruned.
    pub async fn retry_blob(&self, id: Uuid) -> Result<Blob, RetryError> {
        let now = Timestamp::now();
        match repo::retry(self.pool(), id, now).await? {
            repo::Retry::Enqueued(blob, kinds) => {
                tracing::info!(blob = %id, ?kinds, "retrying from the blob");
                Ok(blob)
            }
            repo::Retry::Nothing(blob) => {
                let state = blob.state_at(now);
                if state.is_terminal() {
                    Err(RetryError::Terminal(state))
                } else {
                    Err(RetryError::Nothing(state))
                }
            }
            repo::Retry::Pending(_) => Err(RetryError::Pending),
            repo::Retry::Missing => Err(RetryError::NotFound),
        }
    }

    /// Soft delete of an unattached blob: the objects are purged
    /// asynchronously. `false` when there is no such blob; deleting twice
    /// is not an error.
    pub async fn delete_blob(&self, id: Uuid) -> Result<bool, DeleteError> {
        let purge_at = self.config().upload.purge_at(Timestamp::now());
        match repo::soft_delete(self.pool(), id, purge_at).await? {
            repo::Delete::Done => {
                // This process stops serving it at once; another does
                // within the cache TTL.
                self.heads().invalidate(id);
                Ok(true)
            }
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

/// Where the bytes come from: the shape of the `PUT` body, or, for `url`,
/// a download the service performs instead of a `PUT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputType {
    Raw,
    Multipart,
    /// Fetched by a job from the URL given at creation. See DESIGN.md,
    /// "Fetched blobs".
    Url,
    Rows,
    Archive,
    /// A PDF compiled by typst from a workspace of files. See DESIGN.md,
    /// "Documents".
    Document,
}

agrafe_types::str_enum!(InputType {
    Raw => "raw",
    Multipart => "multipart",
    Url => "url",
    Rows => "rows",
    Archive => "archive",
    Document => "document",
});

impl InputType {
    /// `raw` and `multipart` carry the file itself in the `PUT` body and
    /// need a declared checksum and size.
    pub fn is_direct(self) -> bool {
        matches!(self, InputType::Raw | InputType::Multipart)
    }

    /// `rows`, `archive` and `document` are generated server side from a
    /// source sent in parts.
    pub fn is_generated(self) -> bool {
        matches!(
            self,
            InputType::Rows | InputType::Archive | InputType::Document
        )
    }

    /// A `url` blob's bytes are fetched by a job; there is no `PUT`.
    pub fn is_fetched(self) -> bool {
        matches!(self, InputType::Url)
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
    /// Turned off when the blob was created; never rendered, never
    /// regenerated.
    Disabled,
}

agrafe_types::str_enum!(RepresentationState {
    Pending => "pending",
    Ready => "ready",
    Failed => "failed",
    Unavailable => "unavailable",
    Disabled => "disabled",
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

    pub fn digest_column(self) -> &'static str {
        match self {
            Representation::Thumbnail => "thumbnail_sha256",
            Representation::Preview => "preview_sha256",
        }
    }

    pub fn size_column(self) -> &'static str {
        match self {
            Representation::Thumbnail => "thumbnail_size",
            Representation::Preview => "preview_size",
        }
    }
}

/// What the row records of a rendered representation: enough to answer a
/// conditional or `HEAD` request without reading the object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rendered {
    pub sha256: [u8; 32],
    pub size: u64,
}

impl Rendered {
    fn from_columns(sha256: Option<&[u8]>, size: Option<i32>) -> Option<Self> {
        Some(Self {
            sha256: sha256?.try_into().ok()?,
            size: u64::try_from(size?).ok()?,
        })
    }
}

/// What a preview was rendered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewSource {
    Image,
    EmbeddedImage,
    /// The geometry of a GeoJSON file drawn over a map background.
    GeoJson,
}

agrafe_types::str_enum!(PreviewSource {
    Image => "image",
    EmbeddedImage => "embedded_image",
    GeoJson => "geojson",
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
    /// MD5 of the same bytes, the checksum Active Storage would hold for
    /// the file: declared at creation and checked when the backend gave
    /// it, computed by the upload or the build otherwise.
    pub checksum_md5: Option<Vec<u8>>,
    pub size: Option<i64>,
    pub skip_virus_scan: bool,
    /// Where a `url` blob is fetched from, while it is `created`: cleared
    /// once the bytes are in. Private metadata only: a presigned URL is a
    /// secret, and it never goes into an error or a log.
    pub url: Option<String>,
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

    /// Source parts accepted so far, for a generated blob; the next one
    /// must be `source_parts + 1`. Always zero for a direct blob.
    pub source_parts: i32,
    /// Lines across the parts: manifest entries for an archive or a
    /// document.
    pub source_rows: i64,
    /// Bytes of source across the parts.
    pub source_size: i64,
    /// When the last part landed.
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub source_updated_at: Option<Timestamp>,
    /// Index, from zero, of the sheet of a `rows` blob the source is
    /// filling, and the lines accepted into it so far: the row limit is per
    /// sheet. Zero for anything else.
    pub source_sheet: i32,
    pub source_sheet_rows: i64,
    /// The sheets of a `rows` blob in workbook order, each with its columns
    /// in order: declared at creation, read by accept and build.
    pub source_columns: Option<sqlx::types::Json<Vec<Sheet>>>,
    /// Set by the archive build: members written, and members skipped
    /// because they were gone by then.
    pub archive_entries: Option<i64>,
    pub archive_errors: Option<i64>,
    /// The entry file of a `document` blob's workspace, relative to its
    /// root: declared at creation, read by the build.
    pub source_main: Option<String>,
    /// Bytes the workspace of a `document` blob takes once written out,
    /// accumulated by accept and bounded by `document.max_workspace_size`.
    /// Zero for anything else.
    pub workspace_size: i64,

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
    /// The SHA-256 and size of each rendered WebP; see [`Rendered`].
    pub thumbnail_sha256: Option<Vec<u8>>,
    pub thumbnail_size: Option<i32>,
    pub preview_sha256: Option<Vec<u8>>,
    pub preview_size: Option<i32>,

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
    /// When the job that was to finish the blob ran out of attempts, and
    /// what it said; cleared when another is enqueued. Outlives the dead
    /// job, which the reaper prunes.
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub failed_at: Option<Timestamp>,
    pub last_error: Option<String>,
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
        state_at(self.state, self.expires_at, now)
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

    /// The row as the read routes see it, for a handler that holds the
    /// whole blob and answers as the public port does.
    pub fn head(&self) -> BlobHead {
        BlobHead {
            id: self.id,
            state: self.state,
            filename: self.filename.clone(),
            content_type: self.content_type.clone(),
            category: self.category,
            checksum_sha256: self.checksum_sha256.clone(),
            size: self.size,
            expires_at: self.expires_at,
            scan_state: self.scan_state,
            storage_key: self.storage_key.clone(),
            kek_id: self.kek_id.clone(),
            wrapped_key: self.wrapped_key.clone(),
            encryption_header: self.encryption_header.clone(),
            chunk_size: self.chunk_size,
            thumbnail_state: self.thumbnail_state,
            preview_state: self.preview_state,
            thumbnail_variant_version: self.thumbnail_variant_version,
            preview_variant_version: self.preview_variant_version,
            thumbnail_sha256: self.thumbnail_sha256.clone(),
            thumbnail_size: self.thumbnail_size,
            preview_sha256: self.preview_sha256.clone(),
            preview_size: self.preview_size,
            uploaded_at: self.uploaded_at,
        }
    }

    /// Object key of source part `n` of a generated blob.
    pub fn source_key(id: Uuid, n: u32) -> String {
        format!("{id}/source/{n}")
    }

    /// The sheets of a `rows` blob, in workbook order: at least one.
    pub fn sheets(&self) -> Option<&[Sheet]> {
        self.source_columns
            .as_ref()
            .map(|sheets| sheets.0.as_slice())
            .filter(|sheets| !sheets.is_empty())
    }
}

pub fn public_upload_allowed(skip_virus_scan: bool, input_type: InputType) -> bool {
    !skip_virus_scan && input_type.is_direct()
}

/// See [`Blob::state_at`].
fn state_at(state: BlobState, expires_at: Option<Timestamp>, now: Timestamp) -> BlobState {
    match expires_at {
        Some(at) if at <= now && !state.is_terminal() => BlobState::Expired,
        _ => state,
    }
}

/// What serving a blob needs of its row: identity, state, the envelope and
/// the representation states. `Blob` carries the upload, scan and source
/// bookkeeping besides, including a `rows` blob's column declaration, which
/// is up to megabytes of JSON a download has no use for. The read routes
/// fetch this and nothing more; see `repo::find_head`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BlobHead {
    pub id: Uuid,
    #[sqlx(try_from = "String")]
    pub state: BlobState,
    pub filename: String,
    pub content_type: String,
    #[sqlx(try_from = "String")]
    pub category: Category,
    pub checksum_sha256: Option<Vec<u8>>,
    pub size: Option<i64>,
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub expires_at: Option<Timestamp>,
    #[sqlx(try_from = "String")]
    pub scan_state: ScanState,
    pub storage_key: Option<String>,
    pub kek_id: Option<String>,
    pub wrapped_key: Option<Vec<u8>>,
    pub encryption_header: Option<Vec<u8>>,
    pub chunk_size: Option<i32>,
    #[sqlx(try_from = "String")]
    pub thumbnail_state: RepresentationState,
    #[sqlx(try_from = "String")]
    pub preview_state: RepresentationState,
    pub thumbnail_variant_version: Option<i32>,
    pub preview_variant_version: Option<i32>,
    pub thumbnail_sha256: Option<Vec<u8>>,
    pub thumbnail_size: Option<i32>,
    pub preview_sha256: Option<Vec<u8>>,
    pub preview_size: Option<i32>,
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub uploaded_at: Option<Timestamp>,
}

impl BlobHead {
    /// What the row records of the rendered `kind`, when it records both
    /// the digest and the size.
    pub fn rendered(&self, kind: Representation) -> Option<Rendered> {
        match kind {
            Representation::Thumbnail => {
                Rendered::from_columns(self.thumbnail_sha256.as_deref(), self.thumbnail_size)
            }
            Representation::Preview => {
                Rendered::from_columns(self.preview_sha256.as_deref(), self.preview_size)
            }
        }
    }

    /// See [`Blob::state_at`].
    pub fn state_at(&self, now: Timestamp) -> BlobState {
        state_at(self.state, self.expires_at, now)
    }

    /// See [`Blob::visible_on_public`].
    pub fn visible_on_public(&self, now: Timestamp) -> bool {
        !self.state_at(now).is_terminal()
    }

    /// See [`Blob::is_readable`].
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_upload_rule() {
        assert!(public_upload_allowed(false, InputType::Raw));
        assert!(public_upload_allowed(false, InputType::Multipart));
        assert!(!public_upload_allowed(true, InputType::Raw));
        assert!(!public_upload_allowed(false, InputType::Url));
        assert!(!public_upload_allowed(false, InputType::Rows));
        assert!(!public_upload_allowed(false, InputType::Archive));
        assert!(!public_upload_allowed(false, InputType::Document));
    }

    #[test]
    fn input_types_are_direct_fetched_or_generated() {
        for t in InputType::ALL {
            let classes = [t.is_direct(), t.is_fetched(), t.is_generated()];
            assert_eq!(classes.iter().filter(|c| **c).count(), 1, "{t}");
        }
        assert!(InputType::Url.is_fetched());
        assert!(InputType::Rows.is_generated());
        assert!(InputType::Document.is_generated());
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
