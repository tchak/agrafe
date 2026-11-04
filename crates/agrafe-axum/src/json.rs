//! Response bodies.

use std::collections::BTreeMap;

use serde::Serialize;
use uuid::Uuid;

use agrafe::blob::metadata::RepresentationMetadata;
use agrafe::blob::{BlobHead, BlobState, Representation, ScanState};
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
    /// The same object as the private metadata's, for the kinds the
    /// public port serves, which today are all of them.
    pub representations: BTreeMap<Representation, RepresentationMetadata>,
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
            representations: RepresentationMetadata::all(&blob.representations),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub items: Vec<BlobResponse>,
    pub next_cursor: Option<Uuid>,
}
