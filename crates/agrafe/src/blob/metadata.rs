//! A blob's full metadata as the backend sees it: the body of
//! `GET /blobs/:id` on the private port and the `blob` of a webhook
//! delivery, one shape for both so the backend applies what it gets from
//! either without a second parser. See [DESIGN.md, "Webhooks"].
//!
//! [DESIGN.md, "Webhooks"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#webhooks

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jiff::Timestamp;
use serde::Serialize;
use uuid::Uuid;

use super::envelope::Envelope;
use super::urls::BlobUrls;
use super::{
    Blob, BlobState, ImageFormat, InputType, PreviewSource, Representation, RepresentationRow,
    RepresentationState, Representations, ScanState,
};
use crate::app::Agrafe;
use crate::attachment::Attachment;
use crate::classify::Category;
use crate::jobs::Priority;

impl Agrafe {
    /// The full metadata of `blob`, its representations and attachments
    /// read now.
    pub async fn blob_metadata(&self, blob: &Blob) -> sqlx::Result<BlobMetadata> {
        let ids = [blob.id];
        let (mut representations, attachments) =
            futures::try_join!(self.representations_for(&ids), self.attachments_for(&ids))?;
        let representations = representations.remove(&blob.id).unwrap_or_default();
        Ok(BlobMetadata::new(
            blob,
            &representations,
            &attachments,
            self.urls(blob, &representations),
        ))
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
    /// The derived representations the blob asked for, by kind. A kind
    /// left out at creation is absent, which is `disabled`.
    pub representations: BTreeMap<Representation, RepresentationMetadata>,
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
    /// Set when the job that was to finish the blob, a fetch, a scan, a build
    /// or a render, ran out of attempts: the blob is stuck where it is until
    /// `POST /blobs/:id/retry` or a delete. Cleared when a job is enqueued
    /// for it again. See [DESIGN.md, "Jobs"].
    ///
    /// [DESIGN.md, "Jobs"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#jobs
    pub failed_at: Option<Timestamp>,
    /// What the dead job last said, set and cleared with `failed_at`.
    pub last_error: Option<String>,
    /// The owners using the blob, oldest first, at most
    /// `attachment::INLINE_LIMIT` of them. A blob with any has no
    /// `expires_at`.
    pub attachments: Vec<AttachmentMetadata>,
    /// More owners use the blob than `attachments` holds: the listing by
    /// `blob_id` has them all.
    pub attachments_truncated: bool,
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
    /// The metadata of `blob`, with its `representations`, its
    /// `attachments` and the `urls` minted for it. Past
    /// `attachment::INLINE_LIMIT` the attachments are cut short and say so.
    pub fn new(
        blob: &Blob,
        representations: &Representations,
        attachments: &[Attachment],
        urls: BlobUrls,
    ) -> Self {
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
            representations: RepresentationMetadata::all(representations),
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
            attachments: attachments
                .iter()
                .take(crate::attachment::INLINE_LIMIT)
                .map(AttachmentMetadata::new)
                .collect(),
            attachments_truncated: attachments.len() > crate::attachment::INLINE_LIMIT,
            urls,
            envelope: Envelope::from_head(&blob.head(representations.clone()), Timestamp::now())
                .map(|e| e.encode()),
        }
    }
}

/// One derived representation, as the metadata lists it under its kind.
/// The state always; with `ready`, the digest and size of what was
/// rendered, which a backend storing envelopes gets in encoded form; and
/// what the kind records of its own, see [`RepresentationDetail`].
#[derive(Debug, Serialize)]
pub struct RepresentationMetadata {
    /// Where the rendering stands; never `disabled`, that kind being
    /// absent.
    pub state: RepresentationState,
    /// The format the picture is served in, `webp` or `jxl`, with `ready`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<ImageFormat>,
    /// SHA-256 of the rendered picture, base64, with `ready`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// Size of the rendered picture in bytes, with `ready`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
    /// The WebP stored beside a JPEG XL picture (`media.webp_fallback`),
    /// with `ready`, when there is one: what `?format=webp`, or a client
    /// whose `Accept` leaves JPEG XL out, is served.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<FallbackMetadata>,
    /// What the kind records besides.
    #[serde(flatten)]
    pub detail: RepresentationDetail,
}

/// The WebP fallback of a picture, as the metadata lists it.
#[derive(Debug, Serialize)]
pub struct FallbackMetadata {
    /// Always `webp`, the one format every browser decodes.
    pub format: ImageFormat,
    /// SHA-256 of the WebP, base64.
    pub checksum: String,
    /// Size of the WebP in bytes.
    pub size: i64,
}

/// What a kind records of its own, beyond its state and what was
/// rendered.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RepresentationDetail {
    /// A thumbnail records nothing more.
    Thumbnail {},
    /// What the preview was rendered from; null without a rendered one.
    Preview {
        /// A [`PreviewSource`] value.
        source: Option<PreviewSource>,
    },
}

impl RepresentationMetadata {
    /// The metadata of `row`.
    pub fn new(row: &RepresentationRow) -> Self {
        Self {
            state: row.state,
            format: row.rendered.map(|r| r.format),
            checksum: row.rendered.map(|r| STANDARD.encode(r.sha256)),
            size: row.rendered.and_then(|r| i64::try_from(r.size).ok()),
            // Kept on a pending row for the render to replace or remove,
            // which is not the client's business; listed with the picture.
            fallback: row.rendered.and(row.fallback).and_then(|f| {
                Some(FallbackMetadata {
                    format: ImageFormat::Webp,
                    checksum: STANDARD.encode(f.sha256),
                    size: i64::try_from(f.size).ok()?,
                })
            }),
            detail: match row.kind {
                Representation::Thumbnail => RepresentationDetail::Thumbnail {},
                Representation::Preview => RepresentationDetail::Preview { source: row.source },
            },
        }
    }

    /// The metadata of every representation in `representations`, by kind.
    pub fn all(representations: &Representations) -> BTreeMap<Representation, Self> {
        representations
            .iter()
            .map(|row| (row.kind, Self::new(row)))
            .collect()
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
