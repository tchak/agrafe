//! Response bodies.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jiff::Timestamp;
use serde::Serialize;
use uuid::Uuid;

use agrafe::attachment::Attachment;
use agrafe::blob::envelope::Envelope;
use agrafe::blob::urls::BlobUrls;
use agrafe::blob::{Blob, BlobHead, BlobState, InputType, RepresentationState, ScanState};
use agrafe::classify::Category;
use agrafe::jobs::Priority;

/// Full metadata, private port.
#[derive(Debug, Serialize)]
pub struct BlobResponse {
    pub id: Uuid,
    pub state: BlobState,
    pub filename: String,
    pub content_type: String,
    pub category: Category,
    pub input_type: InputType,
    pub checksum: Option<String>,
    /// MD5 of the bytes, base64: the `checksum` of an Active Storage blob,
    /// for a backend keeping records in both systems. As declared, or
    /// computed by the service once the bytes are in. See DESIGN.md,
    /// "Creation".
    pub checksum_md5: Option<String>,
    pub size: Option<i64>,
    /// Where a `url` blob is fetched from. Private metadata only: a
    /// presigned URL is a secret.
    pub url: Option<String>,
    pub skip_virus_scan: bool,
    pub public_upload_allowed: bool,
    pub expires_at: Option<Timestamp>,
    pub priority: Priority,
    pub scan_state: ScanState,
    pub scanned_at: Option<Timestamp>,
    pub scan_signature: Option<String>,
    pub thumbnail_state: RepresentationState,
    pub preview_state: RepresentationState,
    pub preview_source: Option<String>,
    /// The source of a generated blob: parts accepted, lines and bytes
    /// across them. Null for `raw` and `multipart`.
    pub source_parts: Option<i32>,
    pub source_rows: Option<i64>,
    pub source_size: Option<i64>,
    /// Set once an archive is built: members written and members skipped.
    pub archive_entries: Option<i64>,
    pub archive_errors: Option<i64>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub uploaded_at: Option<Timestamp>,
    pub deleted_at: Option<Timestamp>,
    pub purged_at: Option<Timestamp>,
    /// Set when the job that was to finish the blob, a scan, a build or a
    /// render, ran out of attempts: the blob is stuck where it is until
    /// `POST /blobs/:id/retry` or a delete. Cleared when a job is enqueued
    /// for it again. See DESIGN.md, "Jobs".
    pub failed_at: Option<Timestamp>,
    pub last_error: Option<String>,
    /// The records using the blob, oldest first. A blob with any has no
    /// `expires_at`.
    pub attachments: Vec<AttachmentResponse>,
    pub urls: BlobUrls,
    /// What a read of the blob needs of this row, once it is `ready`, as
    /// one opaque string: a backend holding the signing key mints sealed
    /// URLs from it without asking. See DESIGN.md, "Sealed URLs".
    pub envelope: Option<String>,
}

impl BlobResponse {
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
            attachments: attachments.iter().map(AttachmentResponse::new).collect(),
            urls,
            envelope: Envelope::from_head(&blob.head(), Timestamp::now()).map(|e| e.encode()),
        }
    }
}

/// An attachment, private port.
#[derive(Debug, Serialize)]
pub struct AttachmentResponse {
    pub id: Uuid,
    pub blob_id: Uuid,
    pub record_type: String,
    pub record_id: String,
    pub name: String,
    pub created_at: Timestamp,
}

impl AttachmentResponse {
    pub fn new(attachment: &Attachment) -> Self {
        Self {
            id: attachment.id,
            blob_id: attachment.blob_id,
            record_type: attachment.record_type.clone(),
            record_id: attachment.record_id.clone(),
            name: attachment.name.clone(),
            created_at: attachment.created_at,
        }
    }
}

/// Reduced metadata, public port. Enough for a browser to poll after upload.
#[derive(Debug, Serialize)]
pub struct PublicBlobResponse {
    pub id: Uuid,
    pub state: BlobState,
    pub content_type: String,
    pub category: Category,
    pub size: Option<i64>,
    pub scan_state: ScanState,
    pub thumbnail_state: RepresentationState,
    pub preview_state: RepresentationState,
}

impl PublicBlobResponse {
    pub fn new(blob: &BlobHead) -> Self {
        Self {
            id: blob.id,
            state: blob.state,
            content_type: blob.content_type.clone(),
            category: blob.category,
            size: blob.size,
            scan_state: blob.scan_state,
            thumbnail_state: blob.thumbnail_state,
            preview_state: blob.preview_state,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub items: Vec<BlobResponse>,
    pub next_cursor: Option<Uuid>,
}
