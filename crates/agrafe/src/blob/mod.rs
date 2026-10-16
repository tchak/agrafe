//! Blob model and state machine.

pub mod create;
pub mod repo;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::classify::Category;

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

crate::str_enum!(BlobState {
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

crate::str_enum!(InputType {
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

crate::str_enum!(ScanState {
    Pending => "pending",
    Clean => "clean",
    Infected => "infected",
    Skipped => "skipped",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepresentationState {
    Pending,
    Ready,
    Failed,
    Unavailable,
}

crate::str_enum!(RepresentationState {
    Pending => "pending",
    Ready => "ready",
    Failed => "failed",
    Unavailable => "unavailable",
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
    pub write_once: bool,
    pub skip_virus_scan: bool,
    pub expires_at: Option<DateTime<Utc>>,

    #[sqlx(try_from = "String")]
    pub scan_state: ScanState,
    pub scanned_at: Option<DateTime<Utc>>,

    pub storage_key: Option<String>,
    pub version: i32,

    pub kek_id: Option<String>,
    pub wrapped_key: Option<Vec<u8>>,
    pub nonce_prefix: Option<Vec<u8>>,
    pub chunk_size: Option<i32>,

    #[sqlx(try_from = "String")]
    pub thumbnail_state: RepresentationState,
    #[sqlx(try_from = "String")]
    pub preview_state: RepresentationState,
    pub preview_source: Option<String>,
    pub variant_version: Option<i32>,

    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub upload_started_at: Option<DateTime<Utc>>,
    pub uploaded_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
}

impl Blob {
    /// The single rule deciding whether the public port may upload this blob.
    /// Enforced when minting and again in the public `PUT` handler.
    pub fn public_upload_allowed(&self) -> bool {
        public_upload_allowed(self.write_once, self.skip_virus_scan, self.input_type)
    }

    /// The public port answers `404` for blobs in a terminal state, so a leaked
    /// URL reveals nothing once the blob is gone. The private port still sees
    /// them.
    pub fn visible_on_public(&self) -> bool {
        !self.state.is_terminal()
    }

    /// Representations may be served only from `ready`.
    pub fn is_readable(&self) -> bool {
        self.state == BlobState::Ready
    }

    /// States from which a `PUT` may start.
    pub fn accepts_upload(&self) -> bool {
        match self.state {
            BlobState::Created => true,
            BlobState::Ready => !self.write_once,
            _ => false,
        }
    }
}

pub fn public_upload_allowed(
    write_once: bool,
    skip_virus_scan: bool,
    input_type: InputType,
) -> bool {
    write_once && !skip_virus_scan && input_type.is_direct()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_upload_rule() {
        assert!(public_upload_allowed(true, false, InputType::Raw));
        assert!(public_upload_allowed(true, false, InputType::Multipart));
        assert!(!public_upload_allowed(false, false, InputType::Raw));
        assert!(!public_upload_allowed(true, true, InputType::Raw));
        assert!(!public_upload_allowed(true, false, InputType::Rows));
        assert!(!public_upload_allowed(true, false, InputType::Archive));
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
