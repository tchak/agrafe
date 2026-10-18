//! Building an `archive` blob: the manifest parts read back in order, each
//! member streamed into a stored ZIP entry, the ZIP encrypted and uploaded
//! as it is produced through the shared `build` sink. Nothing larger than
//! a small member is held whole in memory. See DESIGN.md, "Build" and
//! "Archives".
//!
//! A member that was `ready` at accept time may be gone by now: it is
//! skipped and listed in an `ERRORS.txt` at the root, and the row counts
//! it. A bucket or database error is ours and fails the attempt, which
//! retries from the source; a failed attempt aborts its multipart upload
//! and leaves nothing behind.

use std::collections::{HashMap, HashSet};

use bytes::Bytes;
use futures::StreamExt;
use jiff::Timestamp;
use uuid::Uuid;

use super::build::zip::{DosTime, ZipWriter};
use super::build::{self, Assemble, BuildError, Counts, Sink};
use super::{Job, JobError};
use crate::app::Agrafe;
use crate::blob::read::{Reader, Stored};
use crate::blob::repo;
use crate::blob::{Blob, BlobState, InputType};
use crate::source::Source;
use crate::source::archive::{ERRORS_FILE, Entry, parse_line};

/// Members fetched ahead of the one being copied: most entries are small,
/// and a serial build would wait longer for the first byte of each object
/// than it spends copying it.
const LOOKAHEAD: usize = 4;

/// Members up to this size are read whole when fetched ahead, so the
/// look-ahead never leaves a response open and idle while a long entry
/// copies, which an object store cuts off after a while and which would
/// fail the attempt. A larger member is opened when its turn comes. Memory
/// for the look-ahead is bounded by `LOOKAHEAD` times this.
const PREFETCH_MAX: u64 = 8 * 1024 * 1024;

/// Manifest lines resolved per database query.
const BATCH: usize = 256;

pub async fn run(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    build::run::<Archive>(state, job).await
}

/// The archive build: every member of the manifest into a stored entry.
pub(crate) struct Archive;

impl Assemble for Archive {
    const INPUT: InputType = InputType::Archive;

    async fn assemble(state: &Agrafe, blob: &Blob, sink: &mut Sink) -> Result<Counts, BuildError> {
        let (entries, skipped) = assemble(state, blob, sink).await?;
        Ok(Counts {
            archive: Some((entries, skipped)),
        })
    }
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
                    body,
                } => {
                    let name = taken.claim(&entry.path);
                    let time = DosTime::from_timestamp(uploaded_at.unwrap_or(now));
                    sink.write(&zip.begin_entry(&name, size, time)?).await?;
                    match body {
                        Body::Loaded(segments) => {
                            for segment in &segments {
                                copy_segment(&mut zip, sink, segment).await?;
                            }
                        }
                        Body::Deferred(stored) => {
                            let mut reader = Reader::open(state, &stored, 0..stored.size).await?;
                            while let Some(segment) = reader.next_segment().await? {
                                copy_segment(&mut zip, sink, &segment).await?;
                            }
                        }
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

/// One plaintext segment of a member into the archive.
async fn copy_segment(
    zip: &mut ZipWriter,
    sink: &mut Sink,
    segment: &[u8],
) -> Result<(), BuildError> {
    zip.data(segment)?;
    sink.write(segment).await
}

/// What became of a manifest entry when the build reached it.
enum Outcome {
    Member {
        size: u64,
        uploaded_at: Option<Timestamp>,
        body: Body,
    },
    /// Gone since accept; the reason code goes to `ERRORS.txt`.
    Skipped(&'static str),
}

/// A member's bytes, by how the look-ahead left them.
enum Body {
    /// At most `PREFETCH_MAX` bytes, read whole ahead of their turn.
    Loaded(Vec<Bytes>),
    /// Larger: opened and streamed when its turn comes.
    Deferred(Stored),
}

/// Fetch the member of `entry`, which `member` is the current row of, if
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
    let size = stored.size;
    let uploaded_at = member.uploaded_at;
    let body = if size <= PREFETCH_MAX {
        let mut reader = Reader::open(state, &stored, 0..size).await?;
        let mut segments = Vec::with_capacity(stored.layout.segment_count(size) as usize);
        while let Some(segment) = reader.next_segment().await? {
            segments.push(segment);
        }
        Body::Loaded(segments)
    } else {
        Body::Deferred(stored)
    };
    Ok((
        entry,
        Outcome::Member {
            size,
            uploaded_at,
            body,
        },
    ))
}

/// The manifest, read back line by line from the stored parts, every line
/// parsed as accept parsed it.
struct Manifest<'a> {
    source: Source<'a>,
}

impl<'a> Manifest<'a> {
    fn new(state: &'a Agrafe, blob: &'a Blob) -> Self {
        Self {
            source: Source::new(state, blob),
        }
    }

    /// Up to `max` entries, fewer at the end of the source and none after.
    async fn next_batch(&mut self, max: usize) -> Result<Vec<Entry>, BuildError> {
        let mut batch = Vec::with_capacity(max);
        while batch.len() < max {
            let Some(line) = self.source.next_line().await? else {
                break;
            };
            let entry = parse_line(&line.bytes)
                .map_err(|reason| self.source.malformed(line.line, reason))?;
            batch.push(entry);
        }
        Ok(batch)
    }
}

/// The names given out so far, compared case-insensitively since
/// `Report.pdf` and `report.pdf` land on the same file when extracted on
/// macOS or Windows.
pub struct Taken {
    names: HashSet<String>,
    /// Where the counter of a path that collided stands, by the path's
    /// lowercase form, so a manifest of ten thousand `facture.pdf` costs ten
    /// thousand claims and not fifty million: without it each claim would
    /// start over from 1 and the pass would be quadratic, half a minute of
    /// CPU on the worker for twenty thousand duplicates.
    counters: HashMap<String, u64>,
}

impl Default for Taken {
    fn default() -> Self {
        Self::new()
    }
}

impl Taken {
    pub fn new() -> Self {
        let mut names = HashSet::new();
        names.insert(ERRORS_FILE.to_lowercase());
        Self {
            names,
            counters: HashMap::new(),
        }
    }

    /// The name `path` gets: itself when free, else `stem-1.ext`,
    /// `stem-2.ext`, ... with the counter before the last extension of the
    /// last component, the first free one. The name is taken in turn.
    pub fn claim(&mut self, path: &str) -> String {
        let lower = path.to_lowercase();
        if self.names.insert(lower.clone()) {
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
        // A name the backend sent itself may sit on the counter; each is
        // skipped once, so the whole pass stays linear in the manifest.
        let next = self.counters.entry(lower).or_insert(1);
        loop {
            let candidate = format!("{dir}{stem}-{next}{ext}");
            *next += 1;
            if self.names.insert(candidate.to_lowercase()) {
                return candidate;
            }
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

    /// Accept caps paths below the ZIP limit by what a counter adds, so a
    /// duplicate of the longest path still has a name the writer takes.
    #[test]
    fn dedupe_of_the_longest_path_fits_the_zip_limit() {
        use crate::source::archive::{MAX_PATH_LEN, ZIP_NAME_LIMIT};
        let longest = format!("{}.pdf", "a".repeat(MAX_PATH_LEN - 4));
        assert_eq!(longest.len(), MAX_PATH_LEN);
        let mut taken = Taken::new();
        assert_eq!(taken.claim(&longest), longest);
        let renamed = taken.claim(&longest);
        assert!(renamed.ends_with("-1.pdf"));
        assert!(renamed.len() <= ZIP_NAME_LIMIT);
        let t = DosTime::from_timestamp(Timestamp::now());
        ZipWriter::new()
            .begin_entry(&renamed, 0, t)
            .expect("fits the name field");
        // The counter is a u64, so no number of duplicates outgrows it.
        let widest = format!("{}-{}.pdf", "a".repeat(MAX_PATH_LEN - 4), u64::MAX);
        assert_eq!(widest.len(), ZIP_NAME_LIMIT);
    }

    /// Every claim of a repeated path costs one candidate, so a manifest of
    /// identical names is linear: fifty thousand of them in well under a
    /// second, where restarting the counter each time took minutes.
    #[test]
    fn dedupe_is_linear_in_the_manifest() {
        let mut taken = Taken::new();
        let started = std::time::Instant::now();
        for i in 0..50_000u64 {
            let name = taken.claim("factures/facture.pdf");
            if i > 0 {
                assert_eq!(name, format!("factures/facture-{i}.pdf"));
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }
}
