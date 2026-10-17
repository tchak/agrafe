//! Sources of generated blobs: the parts a `rows` or `archive` description
//! arrives in, and the finish that seals them and enqueues the build. See
//! DESIGN.md, "Generated blobs".
//!
//! A part goes through the same pipeline as a file (`upload`), with a
//! [`Tap`] that parses it line by line instead of checking a checksum, to
//! `<blob id>/source/<n>`. The blob is `uploading` while the part is in
//! flight and back in `created` after, with the part counted on the row.
//! A failing part leaves nothing of itself, like any failed upload.

pub mod archive;

use agrafe_crypto::Key;
use agrafe_crypto::streaming::Layout;
use bytes::{Buf, Bytes, BytesMut};
use futures::Stream;
use jiff::Timestamp;
use uuid::Uuid;

use crate::app::Agrafe;
use crate::blob::repo::{self, Transition};
use crate::blob::{Blob, BlobState, InputType};
use crate::encryption::Envelope;
use crate::upload::{self, BoxError, Fed, UploadError};

/// A source has at most this many parts.
pub const MAX_PARTS: u32 = 10_000;

/// Longest line a source may hold: a manifest line is a path and an id, a
/// row a page of scalars. The bound keeps the parser's buffer, and a
/// backend's mistake, small.
pub const MAX_LINE_LEN: usize = 1024 * 1024;

impl Agrafe {
    /// Store part `n` of `blob`'s source, which must be the next one. The
    /// stream is the part's lines; every line is checked before the part
    /// counts, per the rules of the blob's input type.
    pub async fn upload_part<S>(&self, blob: Blob, n: u32, body: S) -> Result<Blob, UploadError>
    where
        S: Stream<Item = Result<Bytes, BoxError>> + Unpin,
    {
        prepare_part(&blob, n, Timestamp::now())?;
        // The data key and layout are the blob's once its first part has
        // fixed them, so every part and the built file share one key; a
        // fresh envelope is only for a blob that has none yet.
        let (envelope, layout) = match (&blob.kek_id, &blob.wrapped_key, blob.chunk_size) {
            (Some(kek_id), Some(wrapped), Some(chunk_size)) => {
                let key = self.keks().unwrap(blob.id, kek_id, wrapped)?;
                let layout = usize::try_from(chunk_size)
                    .map_err(|_| agrafe_crypto::Error::InvalidSegmentSize)
                    .and_then(Layout::new)?;
                (
                    Envelope {
                        key,
                        kek_id: kek_id.clone(),
                        wrapped: wrapped.clone(),
                    },
                    layout,
                )
            }
            _ => (self.keks().new_envelope(blob.id), self.keks().layout()),
        };
        let blob = {
            let mut conn = self.pool().acquire().await?;
            let chunk_size = layout.segment_size() as i32;
            match repo::begin_part(&mut conn, &blob, n, &envelope, chunk_size).await? {
                Transition::Done(blob) => blob,
                Transition::Conflict(current) => {
                    return Err(part_refusal(&current, n, Timestamp::now()));
                }
                Transition::Missing => return Err(UploadError::NotFound),
            }
        };
        let state = self.clone();
        let key = envelope.key;
        upload::feed(body, move |fed| {
            store_part(state, blob, n, key, layout, fed)
        })
        .await
    }

    /// Seal the source of `id` and enqueue its build: `created` to
    /// `uploaded`, from which the build job makes it `ready`.
    pub async fn finish_source(&self, id: Uuid) -> Result<Blob, UploadError> {
        let blob = repo::find(self.pool(), id)
            .await?
            .ok_or(UploadError::NotFound)?;
        finish_refusal(&blob, Timestamp::now())?;
        match repo::finish_source(self.pool(), id).await? {
            Transition::Done(blob) => Ok(blob),
            Transition::Conflict(current) => Err(finish_refusal(&current, Timestamp::now())
                .err()
                .unwrap_or(UploadError::Conflict(current.state))),
            Transition::Missing => Err(UploadError::NotFound),
        }
    }

    /// A source that fits one request: part one and the finish in one call,
    /// which is what `PUT /blobs/:id` does for a generated blob.
    pub async fn upload_source<S>(&self, blob: Blob, body: S) -> Result<Blob, UploadError>
    where
        S: Stream<Item = Result<Bytes, BoxError>> + Unpin,
    {
        let blob = self.upload_part(blob, 1, body).await?;
        self.finish_source(blob.id).await
    }
}

/// Whether `blob` can take part `n` at `now`. Run by the HTTP layer before
/// it reads the body, and again by `upload_part`.
pub fn prepare_part(blob: &Blob, n: u32, now: Timestamp) -> Result<(), UploadError> {
    match blob.input_type {
        InputType::Raw | InputType::Multipart => {
            return Err(UploadError::NotGenerated(blob.input_type));
        }
        InputType::Rows => return Err(UploadError::Unsupported(blob.input_type)),
        InputType::Archive => {}
    }
    if !blob.accepts_upload(now) {
        return Err(UploadError::Conflict(blob.state_at(now)));
    }
    let expected = blob.source_parts as u32 + 1;
    if n != expected {
        return Err(UploadError::PartSequence { expected });
    }
    if n > MAX_PARTS {
        return Err(UploadError::TooManyParts(MAX_PARTS));
    }
    Ok(())
}

/// Why `begin_part` found the row in another shape than `prepare_part`
/// did: a part landed or an upload began in between.
fn part_refusal(current: &Blob, n: u32, now: Timestamp) -> UploadError {
    prepare_part(current, n, now)
        .err()
        .unwrap_or(UploadError::Conflict(current.state_at(now)))
}

/// Whether `blob` can be finished at `now`: a generated blob, still
/// `created`, holding at least one part.
fn finish_refusal(blob: &Blob, now: Timestamp) -> Result<(), UploadError> {
    if blob.input_type.is_direct() {
        return Err(UploadError::NotGenerated(blob.input_type));
    }
    if blob.state_at(now) != BlobState::Created {
        return Err(UploadError::Conflict(blob.state_at(now)));
    }
    if blob.source_parts == 0 {
        return Err(UploadError::NoParts);
    }
    Ok(())
}

/// Encrypt and store part `n`, then count it on the row or hand the row
/// back. `blob` is the row as `begin_part` returned it; its
/// `upload_started_at` is this upload's claim on it, as for a file.
async fn store_part(
    state: Agrafe,
    blob: Blob,
    n: u32,
    key: Key,
    layout: Layout,
    body: Fed,
) -> Result<Blob, UploadError> {
    let started = blob
        .upload_started_at
        .expect("begin_part sets upload_started_at");
    let storage_key = Blob::source_key(blob.id, n);
    debug_assert_eq!(
        blob.input_type,
        InputType::Archive,
        "prepare_part admits archives only"
    );
    let tap = archive::Accept::new(state.clone(), &blob);

    let stored = upload::stream_to_store(
        &state,
        blob.id,
        started,
        &storage_key,
        &key,
        layout,
        tap,
        body,
    )
    .await;
    let (_header, accepted) = match stored {
        Ok(stored) => stored,
        Err(err) => {
            upload::hand_back(&state, blob.id, started).await;
            return Err(err);
        }
    };

    let mut conn = state.pool().acquire().await?;
    match repo::complete_part(&mut conn, blob.id, started, n, &accepted).await? {
        Transition::Done(blob) => Ok(blob),
        Transition::Conflict(current) => {
            upload::disown(&state, &current, &storage_key).await;
            Err(UploadError::Conflict(current.state))
        }
        Transition::Missing => {
            upload::delete_quietly(&state, &storage_key).await;
            Err(UploadError::NotFound)
        }
    }
}

/// A source split into lines as its bytes arrive, each line numbered from
/// one within its part. The build reads the stored parts through the same
/// splitter, so what accept let through, the build parses the same way.
#[derive(Debug, Default)]
pub struct Lines {
    buf: BytesMut,
    /// Bytes at the front of `buf` already searched for a newline, so a
    /// line arriving in many chunks is scanned once, not once per chunk.
    scanned: usize,
    /// Number of the next complete line.
    next: u64,
}

/// A line the rules refused, and which one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineError {
    pub line: u64,
    pub reason: String,
}

impl From<LineError> for UploadError {
    fn from(err: LineError) -> Self {
        UploadError::Line {
            line: err.line,
            reason: err.reason,
        }
    }
}

impl Lines {
    pub fn new() -> Self {
        Self {
            buf: BytesMut::new(),
            scanned: 0,
            next: 1,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// The next complete line, without its newline, or `None` until more
    /// bytes arrive. A line longer than `MAX_LINE_LEN` is refused, whether
    /// its end has arrived or not.
    pub fn next_line(&mut self) -> Result<Option<(u64, Bytes)>, LineError> {
        let too_long = || LineError {
            line: self.next,
            reason: format!("line is longer than {MAX_LINE_LEN} bytes"),
        };
        match memchr::memchr(b'\n', &self.buf[self.scanned..]) {
            Some(found) => {
                let at = self.scanned + found;
                if at > MAX_LINE_LEN {
                    return Err(too_long());
                }
                let line = self.buf.split_to(at).freeze();
                self.buf.advance(1);
                self.scanned = 0;
                let n = self.next;
                self.next += 1;
                Ok(Some((n, line)))
            }
            None => {
                self.scanned = self.buf.len();
                if self.buf.len() > MAX_LINE_LEN {
                    return Err(too_long());
                }
                Ok(None)
            }
        }
    }

    /// The bytes ended: a part is whole lines, so nothing may be left.
    pub fn end(&mut self) -> Result<(), LineError> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(LineError {
                line: self.next,
                reason: "last line does not end with a newline".to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(chunks: &[&[u8]]) -> (Vec<(u64, String)>, Result<(), LineError>) {
        let mut splitter = Lines::new();
        let mut out = Vec::new();
        for chunk in chunks {
            splitter.push(chunk);
            while let Some((n, line)) = splitter.next_line().unwrap() {
                out.push((n, String::from_utf8(line.to_vec()).unwrap()));
            }
        }
        (out, splitter.end())
    }

    #[test]
    fn splits_lines_across_chunks() {
        let (got, end) = lines(&[b"ab\nc", b"d\n\ne", b"f\n"]);
        assert_eq!(
            got,
            vec![
                (1, "ab".into()),
                (2, "cd".into()),
                (3, "".into()),
                (4, "ef".into())
            ]
        );
        assert_eq!(end, Ok(()));

        let (got, end) = lines(&[b"ab\nc"]);
        assert_eq!(got, vec![(1, "ab".into())]);
        assert_eq!(end.unwrap_err().line, 2);

        let (got, end) = lines(&[]);
        assert!(got.is_empty());
        assert_eq!(end, Ok(()));
    }

    #[test]
    fn refuses_an_endless_line_early() {
        let mut splitter = Lines::new();
        splitter.push(&vec![b'x'; MAX_LINE_LEN + 1]);
        let err = splitter.next_line().unwrap_err();
        assert_eq!(err.line, 1);
        assert!(err.reason.contains("longer"), "{err:?}");
    }

    /// The bound holds when the line's end arrives in the same chunk, and
    /// a line exactly at the bound passes.
    #[test]
    fn refuses_a_long_line_that_ended() {
        let mut splitter = Lines::new();
        let mut chunk = vec![b'x'; MAX_LINE_LEN + 1];
        chunk.push(b'\n');
        let err = splitter.next_line_after(&chunk).unwrap_err();
        assert_eq!(err.line, 1);
        assert!(err.reason.contains("longer"), "{err:?}");

        let mut splitter = Lines::new();
        let mut chunk = vec![b'x'; MAX_LINE_LEN];
        chunk.push(b'\n');
        let (n, line) = splitter.next_line_after(&chunk).unwrap().unwrap();
        assert_eq!((n, line.len()), (1, MAX_LINE_LEN));
    }

    /// A line that arrives in pieces is found once its newline lands,
    /// whichever chunk brings it, with the search picking up where it left.
    #[test]
    fn finds_a_newline_across_many_chunks() {
        let mut splitter = Lines::new();
        for _ in 0..100 {
            assert_eq!(splitter.next_line_after(&[b'y'; 1000]).unwrap(), None);
        }
        assert_eq!(splitter.scanned, 100_000);
        let (n, line) = splitter.next_line_after(b"z\nq").unwrap().unwrap();
        assert_eq!((n, line.len()), (1, 100_001));
        assert_eq!(splitter.scanned, 0);
        assert_eq!(splitter.next_line().unwrap(), None);
        assert_eq!(splitter.scanned, 1);
    }

    impl Lines {
        fn next_line_after(&mut self, chunk: &[u8]) -> Result<Option<(u64, Bytes)>, LineError> {
            self.push(chunk);
            self.next_line()
        }
    }
}
