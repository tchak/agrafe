//! The periodic housekeeping job: record expiries, expire abandoned
//! sources, schedule the purges that are due, reset stale uploads, prune
//! dead jobs and the tombstones of purged blobs.
//!
//! One `reap` row lives in the table at a time. Each run schedules the
//! next one `jobs.reap_interval` later before doing its work, so a failing
//! run never stops the clock: it is superseded by the one it scheduled,
//! and the error goes to the log. The runner enqueues the first one when
//! it starts; the unique index drops the duplicate when several processes
//! start, or when one restarts while a row is pending.
//!
//! Each step takes at most `BATCH` rows, so one reap is a bounded
//! transaction however many blobs fell due at once (a bulk import's
//! expiry, an outage). A run that filled a batch brings its successor
//! forward to now, and the backlog drains run after run, each committed.

use jiff::Timestamp;
use sqlx::PgExecutor;
use uuid::Uuid;

use jiff_sqlx::ToSqlx;

use super::notify::{self, Event};
use super::{Job, JobError, JobKind, Priority, purge};
use crate::app::Agrafe;
use crate::blob::repo;
use crate::trace::TraceContext;

pub(crate) async fn run(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    if job.kind != JobKind::Reap {
        return Err(JobError::Unhandled(job.kind));
    }
    let now = Timestamp::now();
    schedule(state.pool(), now + state.config().jobs.reap_interval).await?;
    let report = reap(state, now).await?;
    if report.skipped {
        tracing::info!("another reap is running, skipped this one");
    } else {
        tracing::info!(
            expired = report.expired,
            abandoned = report.abandoned,
            purges = report.purges,
            reset = report.reset,
            exhausted = report.exhausted,
            pruned = report.pruned,
            tombstones = report.tombstones,
            backlog = report.backlog,
            "reaped"
        );
        if report.backlog {
            hasten(state.pool(), Timestamp::now()).await?;
        }
    }
    Ok(())
}

/// Bring the pending reap forward to `now`, for a run that left a backlog.
async fn hasten(exec: impl PgExecutor<'_>, now: Timestamp) -> sqlx::Result<()> {
    sqlx::query(
        "WITH hastened AS (\
             UPDATE jobs SET run_at = $2 \
             WHERE kind = $1 AND blob_id IS NULL AND locked_at IS NULL \
               AND failed_at IS NULL AND run_at > $2 \
             RETURNING kind\
         ) \
         SELECT pg_notify($3, kind) FROM hastened",
    )
    .bind(JobKind::Reap.as_str())
    .bind(now.to_sqlx())
    .bind(super::CHANNEL)
    .execute(exec)
    .await?;
    Ok(())
}

/// Advisory lock held for the length of one reap, keyed on the
/// installation (see `jobs::try_xact_lock`). A run longer than
/// `jobs.reap_interval` would otherwise overlap the one it scheduled, on
/// another process, and the two would update the same rows at once.
const LOCK: i32 = 0x7265_6170; // "reap"

/// Enqueue a reap at `run_at`, unless one is already pending. Outside any
/// trace context: a run schedules its successor from inside its own, and
/// each run is to start a trace, not continue every run since the first.
pub async fn schedule(exec: impl PgExecutor<'_>, run_at: Timestamp) -> sqlx::Result<bool> {
    TraceContext::detached(super::enqueue(
        exec,
        JobKind::Reap,
        None,
        Priority::Normal,
        serde_json::json!({}),
        run_at,
    ))
    .await
}

/// What one reap did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Blobs recorded as `expired`.
    pub expired: usize,
    /// `created` blobs expired as abandoned: parts nobody finished, or an
    /// attached blob whose file never came.
    pub abandoned: usize,
    /// Purges enqueued.
    pub purges: u64,
    /// Stale uploads handed back to `created`.
    pub reset: usize,
    /// Jobs declared dead whose last attempt ended its worker.
    pub exhausted: usize,
    /// Dead jobs removed.
    pub pruned: u64,
    /// Rows of purged blobs removed, their attachments and jobs with them.
    pub tombstones: u64,
    /// Another reap was running: this one did nothing.
    pub skipped: bool,
    /// A step filled its batch: there is more to do, and the next reap
    /// runs now.
    pub backlog: bool,
}

/// Rows each step of a reap takes at most: expired, abandoned (each of
/// its two kinds), purges scheduled, uploads reset, dead jobs and
/// tombstones pruned. Ten thousand keeps a reap's transaction to seconds,
/// and a backlog drains at that rate per run, back to back.
pub const BATCH: i64 = 10_000;

/// One pass, in one transaction, as of `now`; or nothing, when another
/// reap holds the lock.
pub async fn reap(state: &Agrafe, now: Timestamp) -> Result<Report, JobError> {
    let config = state.config();
    let mut tx = state.pool().begin().await?;

    if !super::try_xact_lock(&mut tx, LOCK).await? {
        return Ok(Report {
            skipped: true,
            ..Report::default()
        });
    }

    let full = |n: usize| n >= BATCH as usize;
    let expired = repo::expire(&mut tx, now, BATCH).await?;

    // An export whose backend stopped sending parts, or an attached blob
    // whose file never came; the purge below takes any parts with the
    // prefix.
    let abandoned_before = now
        .checked_sub(config.upload.abandoned_after)
        .unwrap_or(Timestamp::MIN);
    let abandoned = repo::expire_abandoned(&mut tx, abandoned_before, BATCH).await?;
    let mut backlog = full(expired.len()) || full(abandoned.len());

    // The backend learns of each expiry from here, so `expired` lags the
    // moment by at most one interval.
    let ids: Vec<Uuid> = expired.iter().chain(&abandoned).copied().collect();
    if config.webhook.wants(Event::Expired) {
        notify::enqueue_each(&mut *tx, &ids, Event::Expired, now).await?;
    }

    // The blobs just expired that hold bytes are purged, after the grace
    // period, and the infected ones past their expiry.
    let purge_at = config.upload.purge_at(now);
    let infected = repo::infected_due_for_purge(&mut tx, now, BATCH).await?;
    backlog |= full(infected.len());
    let mut purges = purge::enqueue_each(&mut *tx, &ids, purge_at, now).await?;
    purges += purge::enqueue_each(&mut *tx, &infected, purge_at, now).await?;

    // A job whose last attempt took its worker down never reached `fail`,
    // and the claim does not take it again: it dies here, with its blob's
    // verdict and webhook as if its handler had failed.
    let exhausted = super::exhausted(&mut tx, config.jobs.lease_timeout, BATCH).await?;
    backlog |= full(exhausted.len());
    for job in &exhausted {
        super::fail(
            &mut tx,
            job,
            "every attempt ended its worker before it finished",
            true,
            None,
            &config.webhook,
            now,
        )
        .await?;
    }

    // A dead purge pruned leaves its blob's objects to nobody: it is
    // scheduled again, due now, its grace long past.
    let dead_before = now
        .checked_sub(config.jobs.keep_failed)
        .unwrap_or(Timestamp::MIN);
    let pruned = super::prune_failed(&mut *tx, dead_before, BATCH).await?;
    backlog |= pruned.count >= BATCH as u64;
    purges += purge::enqueue_each(&mut *tx, &pruned.purges, now, now).await?;
    let pruned = pruned.count;

    let stale_before = now
        .checked_sub(config.upload.stale_after)
        .unwrap_or(Timestamp::MIN);
    let reset = repo::reset_stale_uploads(&mut tx, stale_before, BATCH).await?;
    backlog |= full(reset.len());

    // Tombstones that have answered `GET /blobs/:id` long enough.
    let tombstone_before = now
        .checked_sub(config.blobs.keep_purged)
        .unwrap_or(Timestamp::MIN);
    let tombstones = repo::prune_tombstones(&mut tx, tombstone_before, BATCH).await?;
    backlog |= tombstones >= BATCH as u64;

    tx.commit().await?;
    Ok(Report {
        expired: expired.len(),
        abandoned: abandoned.len(),
        purges,
        reset: reset.len(),
        exhausted: exhausted.len(),
        pruned,
        tombstones,
        skipped: false,
        backlog,
    })
}
