//! Response bodies.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jiff::Timestamp;
use serde::Serialize;
use uuid::Uuid;

use super::urls::BlobUrls;
use crate::blob::{Blob, BlobState, InputType, RepresentationState, ScanState};
use crate::classify::Category;

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
    pub size: Option<i64>,
    pub skip_virus_scan: bool,
    pub public_upload_allowed: bool,
    pub expires_at: Option<Timestamp>,
    pub scan_state: ScanState,
    pub scanned_at: Option<Timestamp>,
    pub scan_signature: Option<String>,
    pub thumbnail_state: RepresentationState,
    pub preview_state: RepresentationState,
    pub preview_source: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub uploaded_at: Option<Timestamp>,
    pub deleted_at: Option<Timestamp>,
    pub purged_at: Option<Timestamp>,
    pub urls: BlobUrls,
}

impl BlobResponse {
    pub fn new(blob: &Blob, urls: BlobUrls) -> Self {
        Self {
            id: blob.id,
            state: blob.state,
            filename: blob.filename.clone(),
            content_type: blob.content_type.clone(),
            category: blob.category,
            input_type: blob.input_type,
            checksum: blob.checksum_sha256.as_deref().map(|c| STANDARD.encode(c)),
            size: blob.size,
            skip_virus_scan: blob.skip_virus_scan,
            public_upload_allowed: blob.public_upload_allowed(),
            expires_at: blob.expires_at,
            scan_state: blob.scan_state,
            scanned_at: blob.scanned_at,
            scan_signature: blob.scan_signature.clone(),
            thumbnail_state: blob.thumbnail_state,
            preview_state: blob.preview_state,
            preview_source: blob.preview_source.clone(),
            created_at: blob.created_at,
            updated_at: blob.updated_at,
            uploaded_at: blob.uploaded_at,
            deleted_at: blob.deleted_at,
            purged_at: blob.purged_at,
            urls,
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
    pub fn new(blob: &Blob) -> Self {
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
