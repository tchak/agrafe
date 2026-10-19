//! Response bodies.

use serde::Serialize;
use uuid::Uuid;

use agrafe::blob::{BlobHead, BlobState, RepresentationState, ScanState};
use agrafe::classify::Category;

/// The full metadata lives in the library, where the webhook delivery
/// builds the same body.
pub use agrafe::blob::metadata::{
    AttachmentMetadata as AttachmentResponse, BlobMetadata as BlobResponse,
};

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
