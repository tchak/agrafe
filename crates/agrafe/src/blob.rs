//! Blob model and state machine.

pub(crate) mod cache;
pub mod create;
pub mod envelope;
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub mod key;
pub mod metadata;
pub mod read;
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub mod repo;
pub mod representation;
pub mod urls;

use std::collections::HashMap;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::app::Agrafe;
use crate::classify::Category;
use crate::jobs::Priority;
pub use agrafe_types::ImageFormat;
pub use repo::{DEFAULT_LIMIT, ListParams, MAX_LIMIT, Page};

/// Why a blob could not be deleted.
#[derive(Debug, thiserror::Error)]
pub enum DeleteError {
    /// Blobs are shared across attachments that do not know of each other,
    /// so a backend acting for one record cannot know the blob is free:
    /// detach instead, and a blob nobody uses expires on its own.
    #[error("blob is attached and cannot be deleted; detach it first")]
    Attached,
    /// The query failed.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

/// Why nothing was enqueued for a blob.
#[derive(Debug, thiserror::Error)]
pub enum RetryError {
    /// No blob has this id.
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
    /// The query failed.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

/// Why a listing was refused.
#[derive(Debug, thiserror::Error)]
pub enum ListError {
    /// `limit` is outside `1..=MAX_LIMIT`; carries `MAX_LIMIT`.
    #[error("limit must be between 1 and {0}")]
    Limit(usize),
    /// The `content_type` filter is not a valid media type, has a
    /// wildcard, or is too long; the message says which.
    #[error("{0}")]
    ContentType(&'static str),
    /// The query failed.
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

    /// The representations of every blob among `ids`, in one query: the
    /// second query of a listing page, never one per blob. A blob with no
    /// row has no entry, its kinds all `disabled`.
    pub async fn representations_for(
        &self,
        ids: &[Uuid],
    ) -> sqlx::Result<HashMap<Uuid, Representations>> {
        repo::representations_for(self.pool(), ids).await
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
            repo::Retry::Pending => Err(RetryError::Pending),
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

/// Lifecycle of a blob. See [DESIGN.md, "State machine"].
///
/// [DESIGN.md, "State machine"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#state-machine
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobState {
    /// The row exists, the original does not: `PUT` is allowed. A
    /// generated blob holds its source parts here, a `url` blob waits for
    /// its fetch.
    Created,
    /// A `PUT`, the fetch of a `url` blob, or a source part is in flight.
    /// A failed or stale one goes back to `created`.
    Uploading,
    /// The bytes are in the bucket, encrypted, and a job finishes the
    /// blob: the scan, or the build of a generated blob. Not readable.
    Uploaded,
    /// Scanned clean, scan skipped, or built. Readable.
    Ready,
    /// The scan found a virus. Never readable; the objects are kept until
    /// the blob is deleted or passes its `expires_at`.
    Infected,
    /// Past `expires_at`. Not readable; the objects are purged.
    Expired,
    /// Soft deleted. The objects are purged; the row stays as a tombstone
    /// until the reaper prunes it.
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
    /// The `PUT` body is the file bytes.
    Raw,
    /// The `PUT` body is `multipart/form-data` with the bytes in one part
    /// named `file`.
    Multipart,
    /// Fetched by a job from the URL given at creation. See [DESIGN.md,
    /// "Fetched blobs"].
    ///
    /// [DESIGN.md, "Fetched blobs"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#fetched-blobs
    Url,
    /// A CSV, XLSX or ODS built from JSONL lines of cell values against
    /// the declared columns. See [DESIGN.md, "Rows"].
    ///
    /// [DESIGN.md, "Rows"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#rows
    Rows,
    /// A ZIP built from other blobs, listed as JSONL `path` and `blob_id`
    /// entries. See [DESIGN.md, "Generated blobs"].
    ///
    /// [DESIGN.md, "Generated blobs"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#generated-blobs
    Archive,
    /// A PDF compiled by typst from a workspace of files. See [DESIGN.md,
    /// "Documents"].
    ///
    /// [DESIGN.md, "Documents"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#documents
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

/// Where the virus scan of a blob stands, tracked apart from its
/// [`BlobState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanState {
    /// Not scanned yet: no bytes, or a scan job is to run.
    Pending,
    /// Scanned, nothing found.
    Clean,
    /// Scanned, a signature matched; the blob is `infected`.
    Infected,
    /// Never scanned: `skip_virus_scan`, set by the backend or implied by
    /// a generated blob.
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
pub(crate) enum ScanOutcome {
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
    pub(crate) fn states(&self) -> (BlobState, ScanState) {
        match self {
            ScanOutcome::Skipped => (BlobState::Ready, ScanState::Skipped),
            ScanOutcome::Clean => (BlobState::Ready, ScanState::Clean),
            ScanOutcome::Infected(_) => (BlobState::Infected, ScanState::Infected),
            ScanOutcome::Deferred => (BlobState::Uploaded, ScanState::Pending),
        }
    }

    pub(crate) fn is_scanned(&self) -> bool {
        matches!(self, ScanOutcome::Clean | ScanOutcome::Infected(_))
    }

    pub(crate) fn signature(&self) -> Option<&str> {
        match self {
            ScanOutcome::Infected(name) => Some(name),
            _ => None,
        }
    }
}

/// Where the rendering of one derived representation stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepresentationState {
    /// To be rendered by a `render` job, from the upload or from the
    /// lazy repair of a missing object or a stale variant version.
    Pending,
    /// Rendered and stored; served in the format the row records.
    Ready,
    /// A renderer rejected the file. Final for this variant version.
    Failed,
    /// Every source declined the file: nothing renders it. Final for
    /// this variant version.
    Unavailable,
    /// Turned off when the blob was created; never rendered, never
    /// regenerated. Never stored: it is the absence of a row in
    /// `representations`, see [`Representations::state`].
    Disabled,
}

agrafe_types::str_enum!(RepresentationState {
    Pending => "pending",
    Ready => "ready",
    Failed => "failed",
    Unavailable => "unavailable",
    Disabled => "disabled",
});

/// The kinds of derived representation a blob can have. The order is the
/// one they are listed in, the pictures first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Representation {
    /// A picture fitted into the `media.thumbnail` box; the category icon
    /// is served in its place when there is none.
    Thumbnail,
    /// A picture of the whole file bounded by `media.preview.max_dimension`.
    Preview,
}

agrafe_types::str_enum!(Representation {
    Thumbnail => "thumbnail",
    Preview => "preview",
});

/// What the row records of a rendered representation: enough to answer a
/// conditional or `HEAD` request without reading the object, and to name
/// what is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rendered {
    /// The format the picture was encoded in, `media.format` when it was
    /// rendered: what its `Content-Type` and download name say.
    pub format: ImageFormat,
    /// SHA-256 of the rendered picture.
    pub sha256: [u8; 32],
    /// Size of the rendered picture in bytes.
    pub size: u64,
}

impl Rendered {
    pub(crate) fn from_columns(
        format: Option<&str>,
        sha256: Option<&[u8]>,
        size: Option<i64>,
    ) -> Option<Self> {
        Some(Self {
            format: format?.parse().ok()?,
            sha256: sha256?.try_into().ok()?,
            size: u64::try_from(size?).ok()?,
        })
    }
}

/// The WebP stored beside a JPEG XL picture for the clients that cannot
/// decode it, `media.webp_fallback`: the same picture, transcoded, at
/// `<kind>.webp`. What the row records of it, as [`Rendered`] records the
/// picture; the format is always WebP, the only one every browser decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebpFallback {
    /// SHA-256 of the WebP.
    pub sha256: [u8; 32],
    /// Size of the WebP in bytes.
    pub size: u64,
}

impl WebpFallback {
    pub(crate) fn from_columns(sha256: Option<&[u8]>, size: Option<i64>) -> Option<Self> {
        Some(Self {
            sha256: sha256?.try_into().ok()?,
            size: u64::try_from(size?).ok()?,
        })
    }

    /// The fallback as what is served: a WebP picture.
    pub fn rendered(self) -> Rendered {
        Rendered {
            format: ImageFormat::Webp,
            sha256: self.sha256,
            size: self.size,
        }
    }
}

/// Which of a representation's objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Object {
    /// The picture, at `<kind>`, in the format the row records.
    Picture,
    /// Its WebP fallback, at `<kind>.webp`, see [`WebpFallback`].
    Fallback,
}

/// One derived representation of a blob, a row of `representations`: one
/// per blob and kind the blob asked for at creation. A kind with no row
/// is `disabled`, see [`Representations`]. See [DESIGN.md, "Derived
/// representations"].
///
/// [DESIGN.md, "Derived representations"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#derived-representations
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepresentationRow {
    /// Which representation this is.
    pub kind: Representation,
    /// Where its rendering stands; never `disabled`.
    pub state: RepresentationState,
    /// The kind's version it was last built with, `media.variant_version`
    /// for the pictures; another one makes the read path regenerate it.
    /// `None` before its first render.
    pub version: Option<i32>,
    /// The digest and size of what was rendered, recorded with `ready` and
    /// cleared with any other state, which `CHECK`s on the table hold.
    pub rendered: Option<Rendered>,
    /// The WebP fallback stored beside the picture, when there is one.
    /// Recorded with the picture; kept, like `version`, through a request
    /// to regenerate, since the object is still in the bucket and the
    /// render that replaces the picture replaces or removes it; served
    /// only with `ready`.
    pub fallback: Option<WebpFallback>,
    /// Preview only: what it was rendered from, `None` without a rendered
    /// preview.
    pub source: Option<PreviewSource>,
    /// Preview only: `true` when it is `failed` because a tool ran out of
    /// time, which is not for the thumbnail to try again with another.
    pub timed_out: bool,
}

/// The representations a blob has rows for, by kind, at most one of
/// each. A kind that is not there was left out of `representations` at
/// creation and is `disabled`: four of the kinds to come are opt-in, and a
/// row per disabled kind would be billions of rows saying nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Representations(Vec<RepresentationRow>);

impl Representations {
    /// The representations in `rows`, in [`Representation`] order.
    pub fn new(mut rows: Vec<RepresentationRow>) -> Self {
        rows.sort_by_key(|row| row.kind);
        Self(rows)
    }

    /// The row of `kind`, when the blob has one.
    pub fn get(&self, kind: Representation) -> Option<&RepresentationRow> {
        self.0.iter().find(|row| row.kind == kind)
    }

    /// The state of `kind`: `disabled` when the blob has no row for it.
    pub fn state(&self, kind: Representation) -> RepresentationState {
        self.get(kind)
            .map_or(RepresentationState::Disabled, |row| row.state)
    }

    /// The version `kind` was last rendered with; `None` before its first
    /// render, or without a row.
    pub fn version(&self, kind: Representation) -> Option<i32> {
        self.get(kind).and_then(|row| row.version)
    }

    /// What the row of `kind` records of its rendering.
    pub fn rendered(&self, kind: Representation) -> Option<Rendered> {
        self.get(kind).and_then(|row| row.rendered)
    }

    /// What the row of `kind` records of the WebP fallback in the bucket,
    /// whatever its state.
    pub fn fallback(&self, kind: Representation) -> Option<WebpFallback> {
        self.get(kind).and_then(|row| row.fallback)
    }

    /// Every row, in [`Representation`] order.
    pub fn iter(&self) -> std::slice::Iter<'_, RepresentationRow> {
        self.0.iter()
    }

    /// Whether a render is to come: some representation is `pending`.
    pub fn any_pending(&self) -> bool {
        self.0
            .iter()
            .any(|row| row.state == RepresentationState::Pending)
    }
}

/// What a preview was rendered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewSource {
    /// The file is an image, decoded and transcoded.
    Image,
    /// The single image embedded in a one-page PDF or a Writer document
    /// (a DOCX or ODT, or one of their variants), pulled out at full
    /// resolution.
    EmbeddedImage,
    /// The geometry of a GeoJSON file drawn over a map background.
    #[serde(rename = "geojson")]
    GeoJson,
    /// The waypoints, routes and tracks of a GPX file drawn over a map
    /// background.
    Gpx,
    /// The placemarks of a KML document, bare or zipped in a KMZ, drawn
    /// over a map background in their KML styles.
    Kml,
    /// The layers of a zipped shapefile, each from its own system, drawn
    /// over a map background.
    Shapefile,
    /// The geometries of a GML document, each from the system its
    /// `srsName` names, drawn over a map background.
    Gml,
}

agrafe_types::str_enum!(PreviewSource {
    Image => "image",
    EmbeddedImage => "embedded_image",
    GeoJson => "geojson",
    Gpx => "gpx",
    Kml => "kml",
    Shapefile => "shapefile",
    Gml => "gml",
});

impl From<agrafe_types::map::Format> for PreviewSource {
    /// The source a map drawn from a file in `format` is recorded as:
    /// one value per format, so the row says what was drawn. A KMZ is
    /// a KML document in a ZIP, and what was drawn is that document.
    fn from(format: agrafe_types::map::Format) -> Self {
        use agrafe_types::map::Format;
        match format {
            Format::GeoJson => PreviewSource::GeoJson,
            Format::Gpx => PreviewSource::Gpx,
            Format::Kml | Format::Kmz => PreviewSource::Kml,
            Format::Shapefile => PreviewSource::Shapefile,
            Format::Gml => PreviewSource::Gml,
        }
    }
}

/// A row of `blobs`: one stored, or to-be-stored, file. See [DESIGN.md,
/// "Blobs"].
///
/// [DESIGN.md, "Blobs"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#blobs
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Blob {
    /// A UUIDv7 minted at creation, so ids sort by creation time; also the
    /// prefix of the blob's object keys.
    pub id: Uuid,
    /// The state as stored; [`Blob::state_at`] gives the one in effect,
    /// which accounts for `expires_at`.
    #[sqlx(try_from = "String")]
    pub state: BlobState,
    /// The name given at creation, trimmed; used in URLs and
    /// `Content-Disposition`.
    pub filename: String,
    /// The media type, normalised to `type/subtype` in lowercase without
    /// parameters.
    pub content_type: String,
    /// What `content_type` classifies as.
    #[sqlx(try_from = "String")]
    pub category: Category,
    /// How the bytes arrive, fixed at creation.
    #[sqlx(try_from = "String")]
    pub input_type: InputType,
    /// SHA-256 of the file, 32 bytes: declared at creation for `raw` and
    /// `multipart`, optionally for `url`, and written by the upload or the
    /// build otherwise.
    pub checksum_sha256: Option<Vec<u8>>,
    /// MD5 of the same bytes, the checksum Active Storage would hold for
    /// the file: declared at creation and checked when the backend gave
    /// it, computed by the upload or the build otherwise.
    pub checksum_md5: Option<Vec<u8>>,
    /// Size of the file in bytes: declared at creation for `raw` and
    /// `multipart`, optionally for `url`, and written by the upload or the
    /// build otherwise.
    pub size: Option<i64>,
    /// No virus scan: the backend vouches for the bytes, or the blob is
    /// generated, which holds only what the service wrote. Such a blob
    /// cannot be uploaded through the public port.
    pub skip_virus_scan: bool,
    /// Where a `url` blob is fetched from, while it is `created`: cleared
    /// once the bytes are in. Private metadata only: a presigned URL is a
    /// secret, and it never goes into an error or a log.
    pub url: Option<String>,
    /// When the blob becomes `expired`. `None` while it is attached: the
    /// first attachment clears it, and losing the last one sets it again.
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub expires_at: Option<Timestamp>,
    /// Copied onto every job enqueued for the blob. Stored as its rank,
    /// like on `jobs`.
    #[sqlx(try_from = "i16")]
    pub priority: Priority,

    /// Where the virus scan stands.
    #[sqlx(try_from = "String")]
    pub scan_state: ScanState,
    /// When the file was scanned, inline or by a job; `None` until then
    /// and for a skipped scan.
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub scanned_at: Option<Timestamp>,
    /// What clamd found, when the blob is `infected`.
    pub scan_signature: Option<String>,

    /// Object key of the original, see [`key::original`].
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
    /// filling, carried across parts because the row limit is per sheet.
    /// Zero for anything else.
    pub source_sheet: i32,
    /// Lines accepted into that sheet so far; `source_rows` stays the
    /// total. Zero for anything else.
    pub source_sheet_rows: i64,
    /// Members the archive build wrote; `None` until it ran.
    pub archive_entries: Option<i64>,
    /// Members the archive build skipped because they were gone by then,
    /// listed in the archive's `ERRORS.txt`; `None` until it ran.
    pub archive_errors: Option<i64>,
    /// The entry file of a `document` blob's workspace, relative to its
    /// root: declared at creation, read by the build.
    pub source_main: Option<String>,
    /// Bytes the workspace of a `document` blob takes once written out,
    /// accumulated by accept and bounded by `document.max_workspace_size`.
    /// Zero for anything else.
    pub workspace_size: i64,

    /// The key-encryption key, among those in `encryption`, that wraps
    /// `wrapped_key`; `None` until the upload completes, or until the
    /// first source part of a generated blob.
    pub kek_id: Option<String>,
    /// The blob's data key, wrapped with the KEK `kek_id` names and bound
    /// to the blob id.
    pub wrapped_key: Option<Vec<u8>>,
    /// Streaming AEAD header (salt and nonce prefix), cached from the object.
    pub encryption_header: Option<Vec<u8>>,
    /// Segment size of the streaming AEAD in bytes, `encryption.chunk_size`
    /// when the blob's key was made, so the setting can change without
    /// rewriting objects.
    pub chunk_size: Option<i32>,

    /// When the row was inserted.
    #[sqlx(try_from = "jiff_sqlx::Timestamp")]
    pub created_at: Timestamp,
    /// When the row last changed, kept by a trigger. A render finishing
    /// moves its own row in `representations`, not this.
    #[sqlx(try_from = "jiff_sqlx::Timestamp")]
    pub updated_at: Timestamp,
    /// When the upload in flight, or the last one, entered `uploading`:
    /// its claim on the row, carried by its heartbeat, its completion and
    /// its reset.
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub upload_started_at: Option<Timestamp>,
    /// Last sign of life from the process running the upload.
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub upload_heartbeat_at: Option<Timestamp>,
    /// When the bytes landed in the bucket: the upload completed, or the
    /// source of a generated blob was finished.
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub uploaded_at: Option<Timestamp>,
    /// When the blob was soft deleted.
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
    /// The dead job's last error, set and cleared with `failed_at`.
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

    /// Blobs are write-once: a `PUT` may start only before any bytes exist.
    pub(crate) fn accepts_upload(&self, now: Timestamp) -> bool {
        self.state_at(now) == BlobState::Created
    }

    /// Whether something of the blob may be in the bucket: the original,
    /// the source parts a generated blob holds while it is still `created`,
    /// or what an upload left that began and never finished, its object
    /// written but its completion lost (`upload::unfinished`). A delete or
    /// an expiry of such a blob needs a purge.
    pub(crate) fn holds_objects(&self) -> bool {
        self.storage_key.is_some() || self.source_parts > 0 || self.upload_started_at.is_some()
    }

    /// The row as the read routes see it, with the blob's
    /// `representations` read beside it, for a handler that holds the
    /// whole blob and answers as the public port does.
    pub fn head(&self, representations: Representations) -> BlobHead {
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
            uploaded_at: self.uploaded_at,
            representations,
        }
    }
}

/// The public upload rule: only a `raw` or `multipart` blob that will be
/// scanned may be uploaded through the public port. See
/// [`Blob::public_upload_allowed`].
pub fn public_upload_allowed(skip_virus_scan: bool, input_type: InputType) -> bool {
    !skip_virus_scan && input_type.is_direct()
}

/// See [`Blob::state_at`].
pub(crate) fn state_at(
    state: BlobState,
    expires_at: Option<Timestamp>,
    now: Timestamp,
) -> BlobState {
    match expires_at {
        Some(at) if at <= now && !state.is_terminal() => BlobState::Expired,
        _ => state,
    }
}

/// What serving a blob needs of its row: identity, state, the envelope and
/// its representations. `Blob` carries the upload, scan and source
/// bookkeeping besides, which a download has no use for. The read routes
/// fetch this and nothing more, the `representations` rows joined to the
/// blob's in the one query; see `repo::find_head`. Neither carries a
/// `rows` blob's column declaration, see `repo::sheets`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BlobHead {
    /// See [`Blob::id`].
    pub id: Uuid,
    /// See [`Blob::state`].
    #[sqlx(try_from = "String")]
    pub state: BlobState,
    /// See [`Blob::filename`].
    pub filename: String,
    /// See [`Blob::content_type`].
    pub content_type: String,
    /// See [`Blob::category`].
    #[sqlx(try_from = "String")]
    pub category: Category,
    /// See [`Blob::checksum_sha256`].
    pub checksum_sha256: Option<Vec<u8>>,
    /// See [`Blob::size`].
    pub size: Option<i64>,
    /// See [`Blob::expires_at`].
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub expires_at: Option<Timestamp>,
    /// See [`Blob::scan_state`].
    #[sqlx(try_from = "String")]
    pub scan_state: ScanState,
    /// See [`Blob::storage_key`].
    pub storage_key: Option<String>,
    /// See [`Blob::kek_id`].
    pub kek_id: Option<String>,
    /// See [`Blob::wrapped_key`].
    pub wrapped_key: Option<Vec<u8>>,
    /// See [`Blob::encryption_header`].
    pub encryption_header: Option<Vec<u8>>,
    /// See [`Blob::chunk_size`].
    pub chunk_size: Option<i32>,
    /// See [`Blob::uploaded_at`].
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub uploaded_at: Option<Timestamp>,
    /// The blob's rows in `representations`, folded in by the query that
    /// read the blob's own.
    #[sqlx(skip)]
    pub representations: Representations,
}

impl BlobHead {
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
