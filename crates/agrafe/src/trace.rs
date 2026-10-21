//! The trace a unit of work belongs to, carried from a request into the
//! jobs it enqueues and from a job into the jobs it enqueues in turn. See
//! DESIGN.md, "Tracing".
//!
//! A [`TraceContext`] is the W3C `traceparent` of the unit running now:
//! the trace it belongs to and an id of its own. The request middleware
//! and the job runner set it for the duration of their work, with
//! [`TraceContext::scope`]; `jobs::enqueue` reads it with
//! [`TraceContext::current`] and stores it on the row, so the job's id
//! names the unit that enqueued it as its parent. Nothing here exports
//! anything: the trace id is a field of the `request` and `job` spans,
//! which ties their log lines together, and the stored `traceparent` is
//! what an exporter will set the job span's parent from.

use std::fmt;
use std::future::Future;

use tracing::Instrument as _;

tokio::task_local! {
    // `None` inside `detached`: a unit that must not be the parent of what
    // it enqueues, under a scope that would otherwise make it so.
    static CURRENT: Option<TraceContext>;
}

/// The `traceparent` of the running unit of work: a request or a job.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceContext {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    sampled: bool,
}

impl TraceContext {
    /// A new trace, of which the caller is the first unit. Sampled: the
    /// decision is the exporter's, and a trace marked unsampled here would
    /// have its jobs dropped by a sampler that follows the parent.
    pub fn root() -> Self {
        Self {
            trace_id: random(),
            span_id: random(),
            sampled: true,
        }
    }

    /// A unit of work whose parent is `traceparent`: its trace and
    /// sampling decision, an id of its own. `None` for a header the W3C
    /// rules say to ignore, in which case the caller starts a root.
    pub fn continue_from(traceparent: &str) -> Option<Self> {
        Self::parse(traceparent).map(|parent| parent.child())
    }

    /// The unit `traceparent` names, as `Display` wrote it. `None` for a
    /// malformed value. Version `00` is exactly 55 characters; a later
    /// version may append fields, which are ignored.
    pub fn parse(traceparent: &str) -> Option<Self> {
        let s = traceparent.trim().as_bytes();
        let fixed = s.get(..55)?;
        let version = hex::<1>(&fixed[0..2])?[0];
        let well_formed = match version {
            0xff => false,
            0x00 => s.len() == 55,
            _ => s.len() == 55 || s[55] == b'-',
        };
        if !well_formed || fixed[2] != b'-' || fixed[35] != b'-' || fixed[52] != b'-' {
            return None;
        }
        let trace_id = hex::<16>(&fixed[3..35])?;
        let span_id = hex::<8>(&fixed[36..52])?;
        let flags = hex::<1>(&fixed[53..55])?[0];
        if trace_id == [0; 16] || span_id == [0; 8] {
            return None;
        }
        Some(Self {
            trace_id,
            span_id,
            sampled: flags & 0x01 != 0,
        })
    }

    /// The next unit in the same trace: a job run under the context its
    /// row stored, which names the unit that enqueued it.
    pub fn child(&self) -> Self {
        Self {
            span_id: random(),
            ..*self
        }
    }

    /// The trace id, 32 lowercase hex digits: what the log lines of a
    /// request and of every job it led to have in common.
    pub fn trace_id(&self) -> impl fmt::Display + use<> {
        Hex(self.trace_id)
    }

    /// The id of this unit within the trace, 16 lowercase hex digits.
    pub fn span_id(&self) -> impl fmt::Display + use<> {
        Hex(self.span_id)
    }

    /// Whether the trace is sampled, as the flags say.
    pub fn sampled(&self) -> bool {
        self.sampled
    }

    /// The context of the unit running in this task, if one was set.
    pub fn current() -> Option<Self> {
        CURRENT.try_with(|current| *current).ok().flatten()
    }

    /// Run `future` as the unit this context is: `current` returns it
    /// there, and jobs enqueued there name it as their parent.
    pub fn scope<F: Future>(self, future: F) -> impl Future<Output = F::Output> {
        CURRENT.scope(Some(self), future)
    }

    /// Run `future` in no context, whatever the task's: `current` returns
    /// `None` there, and a job enqueued there starts a trace when it runs.
    /// For a periodic job scheduling its successor from inside its own
    /// run, which would otherwise chain every run since the first into one
    /// trace.
    pub fn detached<F: Future>(future: F) -> impl Future<Output = F::Output> {
        CURRENT.scope(None, future)
    }
}

/// `tokio::spawn` that keeps the current span and trace context: the task
/// is part of the same unit of work, and what it logs and enqueues says so.
pub(crate) fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(CURRENT.scope(TraceContext::current(), future.in_current_span()))
}

/// The `traceparent` header value, version `00`.
impl fmt::Display for TraceContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "00-{}-{}-{:02x}",
            Hex(self.trace_id),
            Hex(self.span_id),
            u8::from(self.sampled)
        )
    }
}

impl fmt::Debug for TraceContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TraceContext({self})")
    }
}

/// `N` bytes of randomness, never all zero, which W3C reserves for "none".
fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    if getrandom::fill(&mut bytes).is_err() {
        // No entropy source: the ids only have to be unique enough to
        // tell traces apart in a log, which the clock and a v7 UUID's
        // random bits are.
        let uuid = uuid::Uuid::now_v7().into_bytes();
        bytes.copy_from_slice(&uuid[16 - N..]);
    }
    if bytes == [0; N] {
        bytes[N - 1] = 1;
    }
    bytes
}

/// Lowercase hex, as W3C requires; uppercase is malformed.
fn hex<const N: usize>(digits: &[u8]) -> Option<[u8; N]> {
    fn nibble(d: u8) -> Option<u8> {
        match d {
            b'0'..=b'9' => Some(d - b'0'),
            b'a'..=b'f' => Some(d - b'a' + 10),
            _ => None,
        }
    }
    if digits.len() != 2 * N {
        return None;
    }
    let mut bytes = [0; N];
    let (pairs, _) = digits.as_chunks::<2>();
    for (byte, [high, low]) in bytes.iter_mut().zip(pairs) {
        *byte = nibble(*high)? << 4 | nibble(*low)?;
    }
    Some(bytes)
}

struct Hex<const N: usize>([u8; N]);

impl<const N: usize> fmt::Display for Hex<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn round_trips_a_traceparent() {
        let context = TraceContext::parse(EXAMPLE).unwrap();
        assert_eq!(context.to_string(), EXAMPLE);
        assert_eq!(
            context.trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(context.span_id().to_string(), "00f067aa0ba902b7");
        assert!(context.sampled());
        let unsampled = TraceContext::parse(&EXAMPLE.replace("-01", "-00")).unwrap();
        assert!(!unsampled.sampled());
    }

    #[test]
    fn a_continuation_keeps_the_trace_and_takes_an_id_of_its_own() {
        let parent = TraceContext::parse(EXAMPLE).unwrap();
        let child = TraceContext::continue_from(EXAMPLE).unwrap();
        assert_eq!(child.trace_id().to_string(), parent.trace_id().to_string());
        assert_ne!(child.span_id().to_string(), parent.span_id().to_string());
        assert_eq!(child.sampled(), parent.sampled());
        assert_eq!(TraceContext::parse(&child.to_string()), Some(child));

        let root = TraceContext::root();
        assert_ne!(root.trace_id().to_string(), parent.trace_id().to_string());
        assert!(root.sampled());
    }

    #[test]
    fn later_versions_may_append_fields() {
        let later = "cc-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-what-comes";
        let context = TraceContext::parse(later).unwrap();
        assert_eq!(context.to_string(), EXAMPLE, "written back as version 00");
    }

    #[test]
    fn refuses_what_w3c_says_to_ignore() {
        for bad in [
            "",
            "00",
            // Uppercase hex.
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            // All-zero ids.
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            // Version 00 has nothing after the flags; ff is forbidden.
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-x",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            // A later version's extra field needs its dash.
            "cc-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01x",
            // Wrong separators, short ids, not hex.
            "00_4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e473-600f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902bz-01",
        ] {
            assert_eq!(TraceContext::parse(bad), None, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn the_scope_is_what_current_returns_and_spawn_carries_it() {
        assert_eq!(TraceContext::current(), None);
        let context = TraceContext::root();
        let (inside, spawned) = context
            .scope(async {
                (
                    TraceContext::current(),
                    spawn(async { TraceContext::current() }).await,
                )
            })
            .await;
        assert_eq!(inside, Some(context));
        assert_eq!(spawned.unwrap(), Some(context));
        assert_eq!(TraceContext::current(), None);
    }

    #[tokio::test]
    async fn detached_hides_the_scope_and_the_scope_comes_back_after() {
        let context = TraceContext::root();
        let (detached, after) = context
            .scope(async {
                (
                    TraceContext::detached(async { TraceContext::current() }).await,
                    TraceContext::current(),
                )
            })
            .await;
        assert_eq!(detached, None);
        assert_eq!(after, Some(context));
    }
}
