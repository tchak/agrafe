//! Building an `archive` blob: the manifest parts read back in order, each
//! member streamed into a stored ZIP entry, the ZIP encrypted and uploaded
//! in 32 MiB parts as it is produced. Nothing touches local disk and nothing
//! is held whole in memory. See DESIGN.md, "Build" and "Archives".
//!
//! A member that was `ready` at accept time may be gone by now: it is
//! skipped and listed in an `ERRORS.txt` at the root, and the row counts
//! it. A bucket or database error is ours and fails the attempt, which
//! retries from the source; a failed attempt aborts its multipart upload
//! and leaves nothing behind.

pub mod zip;

use std::collections::{HashMap, HashSet};

use agrafe_crypto::Key;
use agrafe_crypto::streaming::{Encryptor, Layout};
use bytes::Bytes;
use futures::StreamExt;
use jiff::Timestamp;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStoreExt, WriteMultipart};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use self::zip::{DosTime, ZipError, ZipWriter};
use super::{Job, JobError};
use crate::app::Agrafe;
use crate::blob::read::{ReadError, Reader, Stored};
use crate::blob::repo::{self, Built, Transition};
use crate::blob::{Blob, BlobState, InputType};
use crate::encryption::UnwrapError;
use crate::source::archive::{ERRORS_FILE, Entry, parse_line};
use crate::source::{LineError, Lines};
use crate::upload::{self, PART_CONCURRENCY};

/// Parts of the multipart upload. S3 allows 10 000 per object; the default
/// 5 MiB would cap an archive at 50 GB.
const BUILD_PART_SIZE: usize = 32 * 1024 * 1024;

/// Members opened ahead of the one being copied: most entries are small,
/// and a serial build would wait longer for the first byte of each object
/// than it spends copying it.
const LOOKAHEAD: usize = 4;

/// Manifest lines resolved per database query.
const BATCH: usize = 256;

/// Why a build attempt failed.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// The stored source does not parse: accept let something through, or
    /// the object is not what accept stored.
    #[error("part {part}, line {line}: {reason}")]
    Source {
        part: u32,
        line: u64,
        reason: String,
    },
    #[error("blob {blob_id} is stored but has no {what}")]
    Incomplete { blob_id: Uuid, what: &'static str },
    #[error(transparent)]
    Key(#[from] UnwrapError),
    #[error("encryption: {0}")]
    Crypto(#[from] agrafe_crypto::Error),
    #[error("reading a source: {0}")]
    Read(#[from] ReadError),
    #[error("object store: {0}")]
    Store(#[from] object_store::Error),
    #[error("writing the archive: {0}")]
    Zip(#[from] ZipError),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

pub async fn run(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    let id = job.blob_id.ok_or(JobError::NoBlob(job.kind))?;
    let Some(blob) = repo::find(state.pool(), id).await? else {
        tracing::info!("blob is gone, nothing to build");
        return Ok(());
    };
    if blob.state != BlobState::Uploaded || blob.input_type != InputType::Archive {
        tracing::info!(state = %blob.state, input_type = %blob.input_type, "blob is not waiting for an archive build");
        return Ok(());
    }
    let Some(built) = build(state, &blob).await? else {
        tracing::warn!("blob left `uploaded` during the build, nothing written");
        return Ok(());
    };
    match repo::complete_build(state.pool(), id, &built).await? {
        Transition::Done(blob) => tracing::info!(
            size = built.size,
            entries = built.archive.map(|a| a.0),
            skipped = built.archive.map(|a| a.1),
            state = %blob.state,
            "archive built"
        ),
        // The blob left `uploaded` in the moment since the last check: only
        // for a terminal state, so the object is nobody's.
        Transition::Conflict(current) => {
            tracing::warn!(state = %current.state, "blob changed state as the build finished");
            upload::delete_quietly(state, &built.storage_key).await;
        }
        Transition::Missing => upload::delete_quietly(state, &built.storage_key).await,
    }
    Ok(())
}

/// Assemble the archive into `<id>/original`. `None` when the blob was no
/// longer `uploaded` at the end: the object is not finished and nothing
/// is written.
async fn build(state: &Agrafe, blob: &Blob) -> Result<Option<Built>, BuildError> {
    let missing = |what| BuildError::Incomplete {
        blob_id: blob.id,
        what,
    };
    let kek_id = blob.kek_id.as_deref().ok_or_else(|| missing("KEK id"))?;
    let wrapped = blob
        .wrapped_key
        .as_deref()
        .ok_or_else(|| missing("wrapped key"))?;
    let layout = blob
        .chunk_size
        .and_then(|s| usize::try_from(s).ok())
        .and_then(|s| Layout::new(s).ok())
        .ok_or_else(|| missing("valid chunk size"))?;
    let key = state.keks().unwrap(blob.id, kek_id, wrapped)?;

    let storage_key = upload::object_key(blob.id);
    let mut sink = Sink::open(state, &storage_key, &key, layout).await?;
    let assembled = assemble(state, blob, &mut sink).await;
    let (entries, skipped) = match assembled {
        Ok(counts) => counts,
        Err(err) => {
            sink.abort().await;
            return Err(err);
        }
    };
    // One last look before the object becomes visible: the key is the
    // blob's, so only a build of a blob still waiting may finish it.
    let still_ours = repo::find(state.pool(), blob.id)
        .await?
        .is_some_and(|b| b.state == BlobState::Uploaded);
    if !still_ours {
        sink.abort().await;
        return Ok(None);
    }
    let (encryption_header, checksum_sha256, size) = sink.finish().await?;
    Ok(Some(Built {
        storage_key,
        encryption_header,
        checksum_sha256,
        size: size as i64,
        archive: Some((entries as i64, skipped as i64)),
    }))
}

/// Write every entry into `sink`. Returns how many members were written
/// and how many skipped.
async fn assemble(state: &Agrafe, blob: &Blob, sink: &mut Sink) -> Result<(u64, u64), BuildError> {
    let mut manifest = Manifest::new(state, blob);
    let mut zip = ZipWriter::new();
    let mut taken = Taken::new();
    let mut errors: Vec<u8> = Vec::new();
    let mut entries = 0u64;
    let mut skipped = 0u64;
    loop {
        let batch = manifest.next_batch(BATCH).await?;
        if batch.is_empty() {
            break;
        }
        let mut ids: Vec<Uuid> = batch.iter().map(|e| e.blob_id).collect();
        ids.sort_unstable();
        ids.dedup();
        let members: HashMap<Uuid, Blob> = repo::find_many(state.pool(), &ids)
            .await?
            .into_iter()
            .map(|b| (b.id, b))
            .collect();
        let now = Timestamp::now();
        let mut opened = futures::stream::iter(batch)
            .map(|entry| {
                let member = members.get(&entry.blob_id).cloned();
                let state = state.clone();
                async move { open_member(&state, entry, member, now).await }
            })
            .buffered(LOOKAHEAD);
        while let Some(opened) = opened.next().await {
            let (entry, outcome) = opened?;
            match outcome {
                Outcome::Member {
                    size,
                    uploaded_at,
                    mut reader,
                } => {
                    let name = taken.claim(&entry.path);
                    let time = DosTime::from_timestamp(uploaded_at.unwrap_or(now));
                    sink.write(&zip.begin_entry(&name, size, time)?).await?;
                    while let Some(segment) = reader.next_segment().await? {
                        zip.data(&segment)?;
                        sink.write(&segment).await?;
                    }
                    sink.write(&zip.end_entry()?).await?;
                    entries += 1;
                }
                Outcome::Skipped(reason) => {
                    errors.extend_from_slice(
                        format!("{}\t{}\t{reason}\n", entry.path, entry.blob_id).as_bytes(),
                    );
                    skipped += 1;
                }
            }
        }
    }
    if !errors.is_empty() {
        let time = DosTime::from_timestamp(Timestamp::now());
        sink.write(&zip.begin_entry(ERRORS_FILE, errors.len() as u64, time)?)
            .await?;
        zip.data(&errors)?;
        sink.write(&errors).await?;
        sink.write(&zip.end_entry()?).await?;
    }
    sink.write(&zip.finish()?).await?;
    Ok((entries, skipped))
}

/// What became of a manifest entry when the build reached it.
enum Outcome {
    Member {
        size: u64,
        uploaded_at: Option<Timestamp>,
        reader: Box<Reader>,
    },
    /// Gone since accept; the reason code goes to `ERRORS.txt`.
    Skipped(&'static str),
}

/// Open the member of `entry`, which `member` is the current row of, if
/// any. A row in another state than `ready` is a skip, not an error: it
/// was ready at accept, and only a delete or an expiry moves it since.
async fn open_member(
    state: &Agrafe,
    entry: Entry,
    member: Option<Blob>,
    now: Timestamp,
) -> Result<(Entry, Outcome), BuildError> {
    let Some(member) = member else {
        return Ok((entry, Outcome::Skipped(BlobState::Deleted.as_str())));
    };
    let state_now = member.state_at(now);
    if state_now != BlobState::Ready {
        return Ok((entry, Outcome::Skipped(state_now.as_str())));
    }
    let stored = Stored::from_blob(&member)?;
    let reader = Reader::open(state, &member, &stored, 0..stored.size).await?;
    Ok((
        entry,
        Outcome::Member {
            size: stored.size,
            uploaded_at: member.uploaded_at,
            reader: Box::new(reader),
        },
    ))
}

/// The manifest, read back part by part through the same line splitter
/// accept used, so every line the build sees is one accept checked.
struct Manifest<'a> {
    state: &'a Agrafe,
    blob: &'a Blob,
    /// The part being read, zero before the first.
    part: u32,
    reader: Option<Reader>,
    lines: Lines,
}

impl<'a> Manifest<'a> {
    fn new(state: &'a Agrafe, blob: &'a Blob) -> Self {
        Self {
            state,
            blob,
            part: 0,
            reader: None,
            lines: Lines::new(),
        }
    }

    /// Up to `max` entries, fewer at the end of the source and none after.
    async fn next_batch(&mut self, max: usize) -> Result<Vec<Entry>, BuildError> {
        let mut batch = Vec::with_capacity(max);
        while batch.len() < max {
            match self.next_entry().await? {
                Some(entry) => batch.push(entry),
                None => break,
            }
        }
        Ok(batch)
    }

    async fn next_entry(&mut self) -> Result<Option<Entry>, BuildError> {
        loop {
            if let Some((line, bytes)) = self.lines.next_line().map_err(|e| self.malformed(e))? {
                let entry = parse_line(&bytes)
                    .map_err(|reason| self.malformed(LineError { line, reason }))?;
                return Ok(Some(entry));
            }
            let reader = match &mut self.reader {
                Some(reader) => reader,
                None => {
                    if self.part >= self.blob.source_parts as u32 {
                        return Ok(None);
                    }
                    self.part += 1;
                    self.lines = Lines::new();
                    self.reader = Some(Reader::open_part(self.state, self.blob, self.part).await?);
                    continue;
                }
            };
            match reader.next_segment().await? {
                Some(segment) => self.lines.push(&segment),
                None => {
                    self.lines.end().map_err(|e| self.malformed(e))?;
                    self.reader = None;
                }
            }
        }
    }

    fn malformed(&self, err: LineError) -> BuildError {
        BuildError::Source {
            part: self.part,
            line: err.line,
            reason: err.reason,
        }
    }
}

/// The names given out so far, compared case-insensitively since
/// `Report.pdf` and `report.pdf` land on the same file when extracted on
/// macOS or Windows.
pub struct Taken(HashSet<String>);

impl Default for Taken {
    fn default() -> Self {
        Self::new()
    }
}

impl Taken {
    pub fn new() -> Self {
        let mut taken = HashSet::new();
        taken.insert(ERRORS_FILE.to_lowercase());
        Self(taken)
    }

    /// The name `path` gets: itself when free, else `stem-1.ext`,
    /// `stem-2.ext`, ... with the counter before the last extension of the
    /// last component, the first free one. The name is taken in turn.
    pub fn claim(&mut self, path: &str) -> String {
        if self.0.insert(path.to_lowercase()) {
            return path.to_string();
        }
        let (dir, file) = match path.rsplit_once('/') {
            Some((dir, file)) => (format!("{dir}/"), file),
            None => (String::new(), path),
        };
        let (stem, ext) = match file.rsplit_once('.') {
            Some((stem, _)) if !stem.is_empty() => (stem, &file[stem.len()..]),
            _ => (file, ""),
        };
        (1u64..)
            .map(|n| format!("{dir}{stem}-{n}{ext}"))
            .find(|candidate| self.0.insert(candidate.to_lowercase()))
            .expect("the counter is unbounded")
    }
}

/// The archive's bytes on their way to the bucket: hashed, encrypted,
/// uploaded in `BUILD_PART_SIZE` parts with a few in flight.
struct Sink {
    writer: WriteMultipart,
    encryptor: Option<Encryptor>,
    header: Vec<u8>,
    hasher: Sha256,
    size: u64,
    out: Vec<u8>,
    storage_key: String,
}

impl Sink {
    async fn open(
        state: &Agrafe,
        storage_key: &str,
        key: &Key,
        layout: Layout,
    ) -> Result<Self, BuildError> {
        let upload = state
            .store()
            .put_multipart(&ObjectPath::from(storage_key))
            .await?;
        let mut writer = WriteMultipart::new_with_chunk_size(upload, BUILD_PART_SIZE);
        let encryptor = Encryptor::new(key, storage_key.as_bytes(), layout);
        let header = encryptor.header().as_bytes().to_vec();
        writer.write(&header);
        Ok(Self {
            writer,
            encryptor: Some(encryptor),
            header,
            hasher: Sha256::new(),
            size: 0,
            out: Vec::new(),
            storage_key: storage_key.to_string(),
        })
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<(), BuildError> {
        self.hasher.update(bytes);
        self.size += bytes.len() as u64;
        self.encryptor
            .as_mut()
            .expect("open until finish")
            .update(bytes, &mut self.out)?;
        self.flush().await
    }

    async fn flush(&mut self) -> Result<(), BuildError> {
        if !self.out.is_empty() {
            self.writer.wait_for_capacity(PART_CONCURRENCY).await?;
            self.writer.put(Bytes::from(std::mem::take(&mut self.out)));
        }
        Ok(())
    }

    /// Seal the stream and complete the upload: the header, the SHA-256 of
    /// the plaintext and its length.
    async fn finish(mut self) -> Result<(Vec<u8>, Vec<u8>, u64), BuildError> {
        self.encryptor
            .take()
            .expect("open until finish")
            .finish(&mut self.out)?;
        self.flush().await?;
        self.writer.finish().await?;
        Ok((self.header, self.hasher.finalize().to_vec(), self.size))
    }

    async fn abort(self) {
        if let Err(err) = self.writer.abort().await {
            tracing::warn!(key = self.storage_key, error = ?err, "could not abort multipart upload");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupes_in_manifest_order_before_the_last_extension() {
        let mut taken = Taken::new();
        assert_eq!(taken.claim("docs/Report.pdf"), "docs/Report.pdf");
        assert_eq!(taken.claim("docs/report.pdf"), "docs/report-1.pdf");
        assert_eq!(taken.claim("docs/report.pdf"), "docs/report-2.pdf");
        // A name a backend sent itself is taken too; the counter skips it.
        assert_eq!(taken.claim("docs/report-3.pdf"), "docs/report-3.pdf");
        assert_eq!(taken.claim("docs/REPORT.PDF"), "docs/REPORT-4.PDF");
        assert_eq!(taken.claim("a.tar.gz"), "a.tar.gz");
        assert_eq!(taken.claim("a.tar.gz"), "a.tar-1.gz");
        assert_eq!(taken.claim("README"), "README");
        assert_eq!(taken.claim("README"), "README-1");
        assert_eq!(taken.claim(".bashrc"), ".bashrc");
        assert_eq!(taken.claim(".bashrc"), ".bashrc-1");
        assert_eq!(taken.claim("errors.txt"), "errors-1.txt", "reserved");
    }
}
