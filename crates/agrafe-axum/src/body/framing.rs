//! A bound on the bytes of a multipart body that are not the file.
//!
//! The multipart parser buffers everything it reads until it finds the
//! first boundary, and again until a part's headers end, rescanning the
//! whole buffer each time. Until the `file` part yields, the upload
//! pipeline's size counter sees nothing, so the route's body limit,
//! `upload.max_size` plus [`MULTIPART_OVERHEAD`], would be the only bound
//! on that buffer, and the time spent rescanning it grows with its square.
//! The [`Meter`] counts the bytes fed to the parser and the bytes of the
//! file it hands out; once the first run ahead of the second by more than
//! the overhead, the body fails. A file of any size passes, its framing
//! and the parts before it may not exceed the overhead.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use futures::Stream;

use agrafe::upload::{BoxError, MULTIPART_OVERHEAD};

/// The most fed to the parser at once. A client sends network-sized
/// chunks, but a body built in memory may be one chunk of any size; cut
/// down, the file bytes the parser holds before handing them out stay far
/// below the overhead.
///
/// The parser reads its stream until it is pending before it parses
/// what it read, so a body whose chunks are all ready, a fast client's or
/// one in memory, would be buffered whole. After each piece the stream
/// yields once, which hands the piece to the parser and the file bytes in
/// it to the pipeline before the next is fed.
const PIECE: usize = 64 * 1024;

/// The bytes fed to the parser and the bytes of the file it handed out.
#[derive(Debug, Clone, Default)]
pub struct Meter(Arc<Counts>);

#[derive(Debug, Default)]
struct Counts {
    fed: AtomicU64,
    delivered: AtomicU64,
}

impl Meter {
    /// `body`, metered.
    pub fn wrap(&self, body: Body) -> Body {
        Body::from_stream(Metered {
            inner: body.into_data_stream(),
            pending: Bytes::new(),
            meter: self.clone(),
            yielded: true,
            done: false,
        })
    }

    /// Record `n` bytes of the file handed to the pipeline.
    pub fn deliver(&self, n: usize) {
        self.0.delivered.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// Record `n` more bytes fed to the parser; whether the bytes that are
    /// not the file are still within the overhead.
    fn feed(&self, n: usize) -> bool {
        let fed = self.0.fed.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        let delivered = self.0.delivered.load(Ordering::Relaxed);
        fed.saturating_sub(delivered) <= MULTIPART_OVERHEAD
    }
}

/// The body failed: its framing exceeded [`MULTIPART_OVERHEAD`].
#[derive(Debug, thiserror::Error)]
#[error("multipart body has more than {MULTIPART_OVERHEAD} bytes outside the `file` part")]
pub struct FramingTooLarge;

/// Whether `err`, or an error it wraps, is [`FramingTooLarge`].
pub fn exceeded(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur = Some(err);
    while let Some(e) = cur {
        if e.is::<FramingTooLarge>() {
            return true;
        }
        cur = e.source();
    }
    false
}

struct Metered<S> {
    inner: S,
    /// What is left of the last chunk, fed `PIECE` at a time.
    pending: Bytes,
    meter: Meter,
    /// Whether the stream yielded since it last fed a piece.
    yielded: bool,
    done: bool,
}

impl<S, E> Stream for Metered<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<BoxError>,
{
    type Item = Result<Bytes, BoxError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this.done {
            return Poll::Ready(None);
        }
        if !this.yielded {
            this.yielded = true;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        while this.pending.is_empty() {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => this.pending = chunk,
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e.into()))),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
        let piece = this.pending.split_to(this.pending.len().min(PIECE));
        if !this.meter.feed(piece.len()) {
            this.done = true;
            return Poll::Ready(Some(Err(FramingTooLarge.into())));
        }
        this.yielded = false;
        Poll::Ready(Some(Ok(piece)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;

    const OVERHEAD: usize = MULTIPART_OVERHEAD as usize;

    async fn drain(meter: &Meter, body: Body, deliver: bool) -> Result<usize, BoxError> {
        let mut stream = meter.wrap(body).into_data_stream();
        let mut total = 0;
        while let Some(piece) = stream.try_next().await? {
            assert!(piece.len() <= PIECE);
            total += piece.len();
            if deliver {
                meter.deliver(piece.len());
            }
        }
        Ok(total)
    }

    #[tokio::test]
    async fn bytes_handed_out_as_the_file_do_not_count() {
        let meter = Meter::default();
        let len = 4 * OVERHEAD;
        assert_eq!(
            drain(&meter, Body::from(vec![0u8; len]), true)
                .await
                .unwrap(),
            len
        );
    }

    #[tokio::test]
    async fn bytes_never_handed_out_stop_at_the_overhead() {
        let meter = Meter::default();
        assert_eq!(
            drain(&meter, Body::from(vec![0u8; OVERHEAD]), false)
                .await
                .unwrap(),
            OVERHEAD
        );

        let meter = Meter::default();
        let err = drain(&meter, Body::from(vec![0u8; OVERHEAD + 1]), false)
            .await
            .unwrap_err();
        assert!(exceeded(err.as_ref()), "{err}");
    }
}
