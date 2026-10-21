//! A blob's full metadata as the backend sees it: the body of
//! `GET /blobs/:id` on the private port and the `blob` of a webhook
//! delivery, one shape for both so the backend applies what it gets from
//! either without a second parser. See [DESIGN.md, "Webhooks"].
//!
//! [DESIGN.md, "Webhooks"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#webhooks

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jiff::Timestamp;
use serde::Serialize;
use uuid::Uuid;

use super::envelope::Envelope;
use super::urls::BlobUrls;
use super::{Blob, BlobState, InputType, RepresentationState, ScanState};
use crate::app::Agrafe;
use crate::attachment::Attachment;
use crate::classify::Category;
use crate::jobs::Priority;

impl Agrafe {
    /// The full metadata of `blob`, its attachments read now.
    pub async fn blob_metadata(&self, blob: &Blob) -> sqlx::Result<BlobMetadata> {
        let attachments = self.attachments_for(&[blob.id]).await?;
        Ok(BlobMetadata::new(blob, &attachments, self.urls(blob)))
    }
}

/// Full metadata.
#[derive(Debug, Serialize)]
pub struct BlobMetadata {
    /// The blob's id.
    pub id: Uuid,
    /// The state as stored: a blob past `expires_at` the reaper has not
    /// reached yet still reports its previous one.
    pub state: BlobState,
    /// The filename given at creation, trimmed.
    pub filename: String,
    /// The normalised media type.
    pub content_type: String,
    /// What the content type classifies as.
    pub category: Category,
    /// How the bytes arrive.
    pub input_type: InputType,
    /// SHA-256 of the bytes, base64: as declared, or computed once the
    /// bytes are in.
    pub checksum: Option<String>,
    /// MD5 of the bytes, base64: the `checksum` of an Active Storage blob,
    /// for a backend keeping records in both systems. As declared, or
    /// computed by the service once the bytes are in. See [DESIGN.md,
    /// "Creation"].
    ///
    /// [DESIGN.md, "Creation"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#creation
    pub checksum_md5: Option<String>,
    /// Size of the file in bytes: as declared, or known once the bytes
    /// are in.
    pub size: Option<i64>,
    /// Where a `url` blob is fetched from, until it has been. Private
    /// metadata only: a presigned URL is a secret.
    pub url: Option<String>,
    /// The bytes are not scanned: the backend vouched for them, or the
    /// blob is generated.
    pub skip_virus_scan: bool,
    /// Whether the public port accepts the upload, computed by the
    /// public upload rule.
    pub public_upload_allowed: bool,
    /// When the blob expires; null while it is attached.
    pub expires_at: Option<Timestamp>,
    /// Where the blob's jobs go in their kind's queue.
    pub priority: Priority,
    /// Where the virus scan stands.
    pub scan_state: ScanState,
    /// When the bytes were scanned; null until then and for a skipped scan.
    pub scanned_at: Option<Timestamp>,
    /// What clamd found, for an `infected` blob. Private metadata only.
    pub scan_signature: Option<String>,
    /// Where the thumbnail's rendering stands.
    pub thumbnail_state: RepresentationState,
    /// Where the preview's rendering stands.
    pub preview_state: RepresentationState,
    /// What the preview was rendered from, a `PreviewSource` value; null
    /// without a rendered preview.
    pub preview_source: Option<String>,
    /// Source parts accepted so far, for a generated blob. Null for
    /// `raw`, `multipart` and `url`.
    pub source_parts: Option<i32>,
    /// Lines across the source parts of a generated blob. Null for `raw`,
    /// `multipart` and `url`.
    pub source_rows: Option<i64>,
    /// Bytes across the source parts of a generated blob. Null for `raw`,
    /// `multipart` and `url`.
    pub source_size: Option<i64>,
    /// Members the archive build wrote; null until an archive is built.
    pub archive_entries: Option<i64>,
    /// Members the archive build skipped because they were gone by then;
    /// null until an archive is built.
    pub archive_errors: Option<i64>,
    /// When the blob was created.
    pub created_at: Timestamp,
    /// When the row last changed.
    pub updated_at: Timestamp,
    /// When the bytes landed in the bucket.
    pub uploaded_at: Option<Timestamp>,
    /// When the blob was soft deleted.
    pub deleted_at: Option<Timestamp>,
    /// When the purge removed the blob's objects from the bucket.
    pub purged_at: Option<Timestamp>,
    /// Set when the job that was to finish the blob, a scan, a build or a
    /// render, ran out of attempts: the blob is stuck where it is until
    /// `POST /blobs/:id/retry` or a delete. Cleared when a job is enqueued
    /// for it again. See [DESIGN.md, "Jobs"].
    ///
    /// [DESIGN.md, "Jobs"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#jobs
    pub failed_at: Option<Timestamp>,
    /// What the dead job last said, set and cleared with `failed_at`.
    pub last_error: Option<String>,
    /// The owners using the blob, oldest first. A blob with any has no
    /// `expires_at`.
    pub attachments: Vec<AttachmentMetadata>,
    /// The signed public URLs of the blob, with the default options.
    pub urls: BlobUrls,
    /// What a read of the blob needs of this row, once it is `ready`, as
    /// one opaque string: a backend holding the signing key mints sealed
    /// URLs from it without asking. See [DESIGN.md, "Sealed URLs"].
    ///
    /// [DESIGN.md, "Sealed URLs"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#sealed-urls
    pub envelope: Option<String>,
}

impl BlobMetadata {
    /// The metadata of `blob`, with its `attachments` and the `urls`
    /// minted for it.
    pub fn new(blob: &Blob, attachments: &[Attachment], urls: BlobUrls) -> Self {
        let generated = blob.input_type.is_generated();
        Self {
            id: blob.id,
            state: blob.state,
            filename: blob.filename.clone(),
            content_type: blob.content_type.clone(),
            category: blob.category,
            input_type: blob.input_type,
            checksum: blob.checksum_sha256.as_deref().map(|c| STANDARD.encode(c)),
            checksum_md5: blob.checksum_md5.as_deref().map(|c| STANDARD.encode(c)),
            size: blob.size,
            url: blob.url.clone(),
            skip_virus_scan: blob.skip_virus_scan,
            public_upload_allowed: blob.public_upload_allowed(),
            expires_at: blob.expires_at,
            priority: blob.priority,
            scan_state: blob.scan_state,
            scanned_at: blob.scanned_at,
            scan_signature: blob.scan_signature.clone(),
            thumbnail_state: blob.thumbnail_state,
            preview_state: blob.preview_state,
            preview_source: blob.preview_source.clone(),
            source_parts: generated.then_some(blob.source_parts),
            source_rows: generated.then_some(blob.source_rows),
            source_size: generated.then_some(blob.source_size),
            archive_entries: blob.archive_entries,
            archive_errors: blob.archive_errors,
            created_at: blob.created_at,
            updated_at: blob.updated_at,
            uploaded_at: blob.uploaded_at,
            deleted_at: blob.deleted_at,
            purged_at: blob.purged_at,
            failed_at: blob.failed_at,
            last_error: blob.last_error.clone(),
            attachments: attachments.iter().map(AttachmentMetadata::new).collect(),
            urls,
            envelope: Envelope::from_head(&blob.head(), Timestamp::now()).map(|e| e.encode()),
        }
    }
}

/// An attachment, as the full metadata lists it.
#[derive(Debug, Serialize)]
pub struct AttachmentMetadata {
    /// The attachment's id, a UUIDv7.
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
    pub created_at: Timestamp,
}

impl AttachmentMetadata {
    /// The metadata of `attachment`.
    pub fn new(attachment: &Attachment) -> Self {
        Self {
            id: attachment.id,
            blob_id: attachment.blob_id,
            owner_kind: attachment.owner_kind.clone(),
            owner_id: attachment.owner_id.clone(),
            role: attachment.role.clone(),
            created_at: attachment.created_at,
        }
    }
}
