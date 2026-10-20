//! The manifest of a `document` blob: the workspace its PDF is compiled
//! from, one file per line, either taken from a blob (`{"path": "...",
//! "blob_id": "..."}`) or given inline (`{"path": "...", "json": ...}` or
//! `{"path": "...", "text": "..."}`), checked as the part arrives. See
//! [DESIGN.md, "Documents"].
//!
//! [DESIGN.md, "Documents"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#documents

use std::collections::{HashMap, HashSet};

use bytes::Bytes;
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::value::RawValue;
use uuid::Uuid;

use super::{LineError, Lines, normalize_relative_path};
use crate::app::Agrafe;
use crate::blob::repo::{self, PartAccepted};
use crate::blob::{Blob, BlobState};
use crate::upload::{Tap, UploadError};

/// Longest workspace path, in bytes, and longest path component: within
/// what the file system the workspace is written to allows, with room to
/// spare under the temporary directory.
pub const MAX_PATH_LEN: usize = 1024;
pub const MAX_COMPONENT_LEN: usize = 255;

/// The entry file when the creation names none.
pub const DEFAULT_MAIN: &str = "main.typ";

/// Members looked up per query at accept time.
const LOOKUP_BATCH: usize = 256;

/// One line of the manifest, as the backend sends it: a path and exactly
/// one of the three sources of its bytes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Line {
    path: String,
    blob_id: Option<Uuid>,
    /// Kept as written: the backend's JSON reaches the template byte for
    /// byte, key order included. A `null` is a value, not an absence.
    #[serde(default, deserialize_with = "present")]
    json: Option<Box<RawValue>>,
    text: Option<String>,
}

/// A field that is there, whatever it holds: serde reads a `null` into an
/// `Option` as missing, and a document's data may well be `null`.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(d).map(Some)
}

/// Where a workspace file's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// The original of a `ready` blob.
    Blob(Uuid),
    /// Bytes carried by the manifest: a JSON value or a text, as written.
    Inline(Vec<u8>),
}

/// One workspace file, its path normalised and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub content: Content,
}

/// Parse and check one line. The message names what is wrong with it.
pub fn parse_line(line: &[u8]) -> Result<Entry, String> {
    let parsed: Line = serde_json::from_slice(line).map_err(|e| {
        format!(
            "expected {{\"path\": ..., \"blob_id\": ...}}, {{\"path\": ..., \"json\": ...}} \
             or {{\"path\": ..., \"text\": ...}}: {e}"
        )
    })?;
    let content = match (parsed.blob_id, parsed.json, parsed.text) {
        (Some(id), None, None) => Content::Blob(id),
        (None, Some(json), None) => Content::Inline(json.get().as_bytes().to_vec()),
        (None, None, Some(text)) => Content::Inline(text.into_bytes()),
        (None, None, None) => return Err("a line needs one of blob_id, json and text".into()),
        _ => return Err("a line takes only one of blob_id, json and text".into()),
    };
    Ok(Entry {
        path: normalize_path(&parsed.path)?,
        content,
    })
}

/// The path a file gets in the workspace: the archive's rules (NFC, `/`
/// separated, relative, no `.` or `..` components, none of the characters
/// the filename rule refuses) within the lengths a file system takes.
pub fn normalize_path(raw: &str) -> Result<String, String> {
    let path = normalize_relative_path(raw, MAX_PATH_LEN)?;
    if path.split('/').any(|c| c.len() > MAX_COMPONENT_LEN) {
        return Err(format!(
            "path has a component longer than {MAX_COMPONENT_LEN} bytes"
        ));
    }
    Ok(path)
}

/// A member named by a line, waiting for its lookup.
struct Pending {
    line: u64,
    blob_id: Uuid,
}

/// The accept pass over one manifest part: lines parsed as they arrive,
/// members looked up in batches, the workspace's size accumulated and
/// bounded.
pub struct Accept {
    state: Agrafe,
    /// `upload.max_size`, the bound on one part.
    max_part: u64,
    /// `document.max_workspace_size`.
    max_workspace: u64,
    /// `document.max_files`.
    max_files: u64,
    /// The workspace's bytes as the parts before this one left them.
    before: u64,
    /// Its files, likewise: the manifest's lines so far.
    files_before: u64,
    lines: Lines,
    pending: Vec<Pending>,
    /// Paths taken in this part: a path twice in a part is refused here,
    /// twice across parts by the build, which sees them all.
    taken: HashSet<String>,
    rows: u64,
    bytes: u64,
    /// What this part adds to the workspace.
    added: u64,
}

impl Accept {
    pub fn new(state: Agrafe, blob: &Blob) -> Self {
        let config = state.config();
        Self {
            max_part: config.upload.max_size.as_u64(),
            max_workspace: config.document.max_workspace_size.as_u64(),
            max_files: config.document.max_files,
            before: blob.workspace_size.max(0) as u64,
            files_before: blob.source_rows.max(0) as u64,
            state,
            lines: Lines::new(),
            pending: Vec::with_capacity(LOOKUP_BATCH),
            taken: HashSet::new(),
            rows: 0,
            bytes: 0,
            added: 0,
        }
    }

    fn add(&mut self, bytes: u64) -> Result<(), UploadError> {
        self.added += bytes;
        if self.before + self.added > self.max_workspace {
            return Err(UploadError::WorkspaceTooLarge(self.max_workspace));
        }
        Ok(())
    }

    /// Resolve the pending lines: every member must exist and be `ready`
    /// now, and the workspace must stay under its limit.
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
        for pending in std::mem::take(&mut self.pending) {
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
            self.add(member.size.unwrap_or(0).max(0) as u64)?;
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
            if self.files_before + self.rows >= self.max_files {
                return Err(UploadError::TooManyFiles(self.max_files));
            }
            if !self.taken.insert(entry.path.clone()) {
                return Err(LineError {
                    line,
                    reason: format!("path `{}` appears twice in the part", entry.path),
                }
                .into());
            }
            match entry.content {
                Content::Blob(blob_id) => self.pending.push(Pending { line, blob_id }),
                Content::Inline(bytes) => self.add(bytes.len() as u64)?,
            }
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
            size: None,
            sheet: None,
            workspace: self.added,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        let id = Uuid::now_v7();
        let entry =
            parse_line(format!(r#"{{"path": "logo.png", "blob_id": "{id}"}}"#).as_bytes()).unwrap();
        assert_eq!(entry.path, "logo.png");
        assert_eq!(entry.content, Content::Blob(id));

        let entry =
            parse_line(r#"{"path": "data.json", "json": {"b": [1, 2], "a": "é"}}"#.as_bytes())
                .unwrap();
        assert_eq!(
            entry.content,
            Content::Inline(r#"{"b": [1, 2], "a": "é"}"#.as_bytes().to_vec()),
            "as written"
        );
        let entry = parse_line(br#"{"path": "n.json", "json": null}"#).unwrap();
        assert_eq!(entry.content, Content::Inline(b"null".to_vec()));

        let entry = parse_line(br##"{"path": "lib/a.typ", "text": "#let x = 1\n"}"##).unwrap();
        assert_eq!(entry.content, Content::Inline(b"#let x = 1\n".to_vec()));

        for bad in [
            &b""[..],
            b"[]",
            br#"{"path": "a"}"#,
            br#"{"path": "a", "blob_id": "nope"}"#,
            br#"{"path": "a", "text": 1}"#,
            br#"{"path": "a", "json": {}, "text": ""}"#,
            br#"{"path": "a", "text": "", "x": 1}"#,
            br#"{"path": "../a", "text": ""}"#,
        ] {
            let text = String::from_utf8_lossy(bad);
            assert!(parse_line(bad).is_err(), "{text}");
        }
        let both = format!(r#"{{"path": "a", "blob_id": "{id}", "json": 1}}"#);
        assert!(
            parse_line(both.as_bytes())
                .unwrap_err()
                .contains("only one")
        );
        assert!(
            parse_line(br#"{"path": "a"}"#)
                .unwrap_err()
                .contains("needs one")
        );
    }

    #[test]
    fn paths() {
        assert_eq!(normalize_path("lib/header.typ").unwrap(), "lib/header.typ");
        assert_eq!(normalize_path("e\u{301}te\u{301}.typ").unwrap(), "été.typ");
        assert!(
            normalize_path("ERRORS.txt").is_ok(),
            "no reserved name here"
        );
        for bad in ["", "/a", "a/../b", "a//b", "a\\b", "dir/", "a\u{202E}b"] {
            assert!(normalize_path(bad).is_err(), "{bad:?}");
        }
        let component = "a".repeat(MAX_COMPONENT_LEN);
        assert!(normalize_path(&component).is_ok());
        assert!(normalize_path(&format!("{component}b")).is_err());
        let deep = format!("{}/x", [component.as_str(); 3].join("/"));
        assert!(normalize_path(&deep).is_ok());
        let long = [component.as_str(); 5].join("/");
        assert!(long.len() > MAX_PATH_LEN);
        assert!(normalize_path(&long).is_err());
    }
}
