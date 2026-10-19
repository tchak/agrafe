//! The manifest of an `archive` blob: one `{"path": "...", "blob_id":
//! "..."}` per line, checked as the part arrives. See DESIGN.md,
//! "Archives".

use std::collections::HashMap;

use bytes::Bytes;
use jiff::Timestamp;
use serde::Deserialize;
use uuid::Uuid;

use super::{LineError, Lines, normalize_relative_path};
use crate::app::Agrafe;
use crate::blob::repo::{self, PartAccepted};
use crate::blob::{Blob, BlobState};
use crate::upload::{Tap, UploadError};

/// The ZIP format's limit on an entry name, in bytes.
pub const ZIP_NAME_LIMIT: usize = 65_535;

/// What a deduplicated name may grow by: `-` and the digits of a counter,
/// which is a `u64`.
pub const DEDUPE_ROOM: usize = 1 + 20;

/// Longest path a manifest may hold, in bytes: the ZIP limit less the
/// room the build needs to rename a duplicate, so accept never lets
/// through a name the build cannot write.
pub const MAX_PATH_LEN: usize = ZIP_NAME_LIMIT - DEDUPE_ROOM;

/// The file the build writes at the root when members were skipped;
/// reserved, so a manifest cannot name it.
pub const ERRORS_FILE: &str = "ERRORS.txt";

/// Bytes an entry costs beyond its member and its name, which appears
/// twice: the local header (30), the data descriptor (up to 24), the
/// central directory header (46) with a zip64 extra (up to 28), and room
/// for the `-n` a deduplicated name grows by.
pub const ENTRY_OVERHEAD: u64 = 160;

/// Members looked up per query at accept time.
const LOOKUP_BATCH: usize = 256;

/// One line of the manifest, as the backend sends it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Line {
    path: String,
    blob_id: Uuid,
}

/// One manifest entry, its path normalised and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub blob_id: Uuid,
}

/// Parse and check one line. The message names what is wrong with it.
pub fn parse_line(line: &[u8]) -> Result<Entry, String> {
    let parsed: Line = serde_json::from_slice(line)
        .map_err(|e| format!("expected {{\"path\": ..., \"blob_id\": ...}}: {e}"))?;
    Ok(Entry {
        path: normalize_path(&parsed.path)?,
        blob_id: parsed.blob_id,
    })
}

/// The path an entry gets in the archive: NFC, `/` separated, relative,
/// no `.` or `..` components, none of the characters the filename rule
/// refuses, at most the ZIP limit, and not the reserved `ERRORS.txt`.
pub fn normalize_path(raw: &str) -> Result<String, String> {
    let path = normalize_relative_path(raw, MAX_PATH_LEN)?;
    if path.to_lowercase() == ERRORS_FILE.to_lowercase() {
        return Err(format!("{ERRORS_FILE} at the root is reserved"));
    }
    Ok(path)
}

/// What an entry adds to the archive: its member, stored, plus the
/// headers around it.
pub fn estimate(path_len: usize, member_size: u64) -> u64 {
    member_size + 2 * path_len as u64 + ENTRY_OVERHEAD
}

/// A member named by a line, waiting for its lookup.
struct Pending {
    line: u64,
    path_len: usize,
    blob_id: Uuid,
}

/// The accept pass over one manifest part: lines parsed as they arrive,
/// members looked up in batches, the archive's size accumulated and
/// bounded.
pub struct Accept {
    state: Agrafe,
    /// `upload.max_size`, the bound on one part.
    max_part: u64,
    /// `archive.max_size`.
    max_archive: u64,
    /// The archive's size as the parts before this one left it.
    size_before: u64,
    lines: Lines,
    pending: Vec<Pending>,
    rows: u64,
    bytes: u64,
    /// What this part adds to the archive's size.
    added: u64,
}

impl Accept {
    pub fn new(state: Agrafe, blob: &Blob) -> Self {
        let config = state.config();
        Self {
            max_part: config.upload.max_size.as_u64(),
            max_archive: config.archive.max_size.as_u64(),
            size_before: blob.size.unwrap_or(0).max(0) as u64,
            state,
            lines: Lines::new(),
            pending: Vec::with_capacity(LOOKUP_BATCH),
            rows: 0,
            bytes: 0,
            added: 0,
        }
    }

    /// Resolve the pending lines: every member must exist and be `ready`
    /// now, and the archive must stay under its limit.
    async fn lookup(&mut self) -> Result<(), UploadError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut ids: Vec<Uuid> = self.pending.iter().map(|p| p.blob_id).collect();
        ids.sort_unstable();
        ids.dedup();
        let members: HashMap<Uuid, Blob> = repo::find_many(self.state.pool(), &ids)
            .await?
            .into_iter()
            .map(|b| (b.id, b))
            .collect();
        let now = Timestamp::now();
        for pending in self.pending.drain(..) {
            let refused = |reason: String| UploadError::Line {
                line: pending.line,
                reason,
            };
            let member = members
                .get(&pending.blob_id)
                .ok_or_else(|| refused(format!("blob {} does not exist", pending.blob_id)))?;
            let state = member.state_at(now);
            if state != BlobState::Ready {
                return Err(refused(format!(
                    "blob {} is {state}, not ready",
                    pending.blob_id
                )));
            }
            let size = member.size.unwrap_or(0).max(0) as u64;
            self.added += estimate(pending.path_len, size);
            if self.size_before + self.added > self.max_archive {
                return Err(UploadError::ArchiveTooLarge(self.max_archive));
            }
        }
        Ok(())
    }
}

impl Tap for Accept {
    type Output = PartAccepted;

    async fn chunk(&mut self, chunk: &Bytes) -> Result<(), UploadError> {
        self.bytes += chunk.len() as u64;
        if self.bytes > self.max_part {
            return Err(UploadError::PartTooLarge(self.max_part));
        }
        self.lines.push(chunk);
        while let Some((line, bytes)) = self.lines.next_line()? {
            let entry = parse_line(&bytes).map_err(|reason| LineError { line, reason })?;
            self.pending.push(Pending {
                line,
                path_len: entry.path.len(),
                blob_id: entry.blob_id,
            });
            self.rows += 1;
            if self.pending.len() >= LOOKUP_BATCH {
                self.lookup().await?;
            }
        }
        Ok(())
    }

    async fn end(mut self) -> Result<PartAccepted, UploadError> {
        self.lines.end()?;
        self.lookup().await?;
        Ok(PartAccepted {
            rows: self.rows,
            bytes: self.bytes,
            size: Some(self.added),
            sheet: None,
            workspace: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(normalize_path("a/b/c.pdf").unwrap(), "a/b/c.pdf");
        // Decomposed é becomes the precomposed one.
        assert_eq!(normalize_path("e\u{301}te\u{301}.pdf").unwrap(), "été.pdf");
        for bad in [
            "",
            "/etc/passwd",
            "a/../b",
            "..",
            ".",
            "a/./b",
            "a//b",
            "dir/",
            "a\\b",
            "a\tb",
            "invoice\u{202E}fdp.exe",
            "ERRORS.txt",
            "errors.TXT",
        ] {
            assert!(normalize_path(bad).is_err(), "{bad:?}");
        }
        assert!(
            normalize_path("sub/ERRORS.txt").is_ok(),
            "reserved at the root only"
        );
        assert!(normalize_path("x.").is_ok());
        assert!(normalize_path(".hidden").is_ok());
        let long = "a".repeat(MAX_PATH_LEN);
        assert!(normalize_path(&long).is_ok());
        assert!(normalize_path(&format!("{long}b")).is_err());
    }

    #[test]
    fn lines() {
        let id = Uuid::now_v7();
        let entry =
            parse_line(format!(r#"{{"path": "a.pdf", "blob_id": "{id}"}}"#).as_bytes()).unwrap();
        assert_eq!(entry.path, "a.pdf");
        assert_eq!(entry.blob_id, id);
        assert!(parse_line(b"").is_err());
        assert!(parse_line(b"[]").is_err());
        assert!(parse_line(br#"{"path": "a.pdf"}"#).is_err());
        assert!(parse_line(br#"{"path": "a.pdf", "blob_id": "nope"}"#).is_err());
        let extra = format!(r#"{{"path": "a.pdf", "blob_id": "{id}", "x": 1}}"#);
        assert!(parse_line(extra.as_bytes()).is_err());
    }
}
