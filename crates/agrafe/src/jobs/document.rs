//! Building a `document` blob: the manifest read back in order and the
//! workspace it describes written to a temporary directory, inline files
//! as they are and blob members decrypted into it; the
//! `agrafe-render-document` helper run on that directory in the sandbox;
//! and the PDF it wrote streamed into the bucket through the shared
//! `build` sink. See [DESIGN.md, "Documents"].
//!
//! A document that does not compile is dead at once, the compiler's
//! diagnostics as the error; so is a member gone since accept, since a
//! template cannot do without a file the way an archive skips an entry,
//! and a compile the sandbox killed at the time limit. A helper that
//! failed any other way (a full disk, a crash), the sandbox, the
//! temporary directory, the bucket and the database may come back, and
//! fail the attempt only.
//!
//! [DESIGN.md, "Documents"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#documents

use std::collections::HashSet;
use std::path::Path;

use agrafe_render::{Rejection, Verdict, helpers};
use jiff::Timestamp;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use super::build::{self, Assemble, BuildError, Counts, Sink};
use super::{Job, JobError};
use crate::app::Agrafe;
use crate::blob::read::{Reader, Stored};
use crate::blob::repo;
use crate::blob::{Blob, BlobState, InputType};
use crate::source::Source;
use crate::source::document::{Content, parse_line};

/// Bytes of the PDF read at a time on their way to the sink.
const CHUNK: usize = 256 * 1024;

pub async fn run(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    build::run::<Document>(state, job).await
}

/// The document build: the workspace compiled into a PDF.
pub(crate) struct Document;

impl Assemble for Document {
    const INPUT: InputType = InputType::Document;

    async fn assemble(state: &Agrafe, blob: &Blob, sink: &mut Sink) -> Result<Counts, BuildError> {
        let media = &state.config().media;
        let workdir = agrafe_render::workdir(media.tmp_dir.as_deref()).map_err(BuildError::Io)?;
        let root = workdir.path().join("workspace");
        tokio::fs::create_dir(&root).await.map_err(BuildError::Io)?;
        write_workspace(state, blob, &root).await?;

        let main = blob.source_main.as_deref().ok_or(BuildError::Incomplete {
            blob_id: blob.id,
            what: "main file",
        })?;
        let output = workdir.path().join("document.pdf");
        match helpers::render_document(&media.render, &root, main, &output).await? {
            Verdict::Rendered(()) => {}
            Verdict::Rejected(Rejection::Failed {
                code: Some(helpers::DOCUMENT_REJECTED_EXIT),
                stderr,
                ..
            }) => return Err(BuildError::Compile(stderr)),
            Verdict::Rejected(Rejection::TimedOut) => {
                return Err(BuildError::Compile(Rejection::TimedOut.to_string()));
            }
            Verdict::Rejected(rejection) => return Err(BuildError::Helper(rejection)),
            Verdict::Declined => {
                return Err(agrafe_render::Error::Output(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "the compiler helper exited cleanly without writing the PDF",
                ))
                .into());
            }
        }

        let mut file = tokio::fs::File::open(&output)
            .await
            .map_err(BuildError::Io)?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = file.read(&mut buf).await.map_err(BuildError::Io)?;
            if n == 0 {
                break;
            }
            sink.write(&buf[..n]).await?;
        }
        Ok(Counts::default())
    }
}

/// Every file of the manifest under `root`, within
/// `document.max_workspace_size` and `document.max_files`.
async fn write_workspace(state: &Agrafe, blob: &Blob, root: &Path) -> Result<(), BuildError> {
    let config = &state.config().document;
    let max = config.max_workspace_size.as_u64();
    let mut source = Source::new(state, blob);
    let mut layout = Layout::default();
    let mut total = 0u64;
    let now = Timestamp::now();
    while let Some(line) = source.next_line().await? {
        let entry =
            parse_line(&line.bytes).map_err(|reason| source.malformed(line.line, reason))?;
        if layout.files.len() as u64 >= config.max_files {
            return Err(BuildError::Workspace(format!(
                "the workspace exceeds the maximum of {} files (document.max_files)",
                config.max_files
            )));
        }
        layout
            .place(&entry.path)
            .map_err(|reason| source.malformed(line.line, reason))?;
        let path = root.join(&entry.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(BuildError::Io)?;
        }
        match &entry.content {
            Content::Inline(bytes) => {
                charge(&mut total, bytes.len() as u64, max)?;
                tokio::fs::write(&path, bytes)
                    .await
                    .map_err(BuildError::Io)?;
            }
            Content::Blob(id) => {
                let stored = find_member(state, &entry.path, *id, now).await?;
                charge(&mut total, stored.size, max)?;
                copy_member(state, &stored, &path).await?;
            }
        }
    }
    Ok(())
}

/// Count `bytes` towards the workspace, within `max`.
fn charge(total: &mut u64, bytes: u64, max: u64) -> Result<(), BuildError> {
    *total += bytes;
    if *total > max {
        return Err(BuildError::Workspace(format!(
            "the workspace exceeds the maximum of {max} bytes (document.max_workspace_size)"
        )));
    }
    Ok(())
}

/// The stored original of a member, which must still be `ready`: it was
/// at accept, and only a delete or an expiry moves it since.
async fn find_member(
    state: &Agrafe,
    path: &str,
    blob_id: Uuid,
    now: Timestamp,
) -> Result<Stored, BuildError> {
    let member = repo::find(state.pool(), blob_id).await?;
    let gone = |state| BuildError::Member {
        path: path.to_string(),
        blob_id,
        state,
    };
    let Some(member) = member else {
        return Err(gone(BlobState::Deleted));
    };
    let state_now = member.state_at(now);
    if state_now != BlobState::Ready {
        return Err(gone(state_now));
    }
    Ok(Stored::from_blob(&member)?)
}

/// Decrypt a member into `path`.
async fn copy_member(state: &Agrafe, stored: &Stored, path: &Path) -> Result<(), BuildError> {
    let mut reader = Reader::open(state, stored, 0..stored.size).await?;
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(BuildError::Io)?;
    while let Some(segment) = reader.next_segment().await? {
        file.write_all(&segment).await.map_err(BuildError::Io)?;
    }
    file.flush().await.map_err(BuildError::Io)?;
    Ok(())
}

/// The paths placed so far, so that a path given twice, or one that is a
/// file on one line and a directory on another, is refused rather than
/// silently overwritten or failed by the file system.
#[derive(Default)]
struct Layout {
    files: HashSet<String>,
    dirs: HashSet<String>,
}

impl Layout {
    fn place(&mut self, path: &str) -> Result<(), String> {
        if self.dirs.contains(path) {
            return Err(format!("`{path}` is both a file and a directory"));
        }
        if !self.files.insert(path.to_string()) {
            return Err(format!("path `{path}` appears twice in the manifest"));
        }
        for (at, _) in path.match_indices('/') {
            let dir = &path[..at];
            if self.files.contains(dir) {
                return Err(format!("`{dir}` is both a file and a directory"));
            }
            self.dirs.insert(dir.to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_refuses_duplicates_and_file_directory_clashes() {
        let mut layout = Layout::default();
        layout.place("main.typ").unwrap();
        layout.place("lib/a.typ").unwrap();
        layout.place("lib/b/c.typ").unwrap();
        assert!(layout.place("main.typ").unwrap_err().contains("twice"));
        assert!(layout.place("lib").unwrap_err().contains("both"));
        assert!(layout.place("lib/b").unwrap_err().contains("both"));
        assert!(layout.place("main.typ/x").unwrap_err().contains("both"));
        layout.place("lib/d.typ").unwrap();
    }
}
