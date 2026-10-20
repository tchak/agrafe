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

use jiff::Timestamp;
use sqlx::PgExecutor;
use uuid::Uuid;

use super::notify::{self, Event};
use super::{Job, JobError, JobKind, Priority};
use crate::app::Agrafe;
use crate::blob::repo;

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
            pruned = report.pruned,
            tombstones = report.tombstones,
            "reaped"
        );
    }
    Ok(())
}

/// Advisory lock held for the length of one reap. A run longer than
/// `jobs.reap_interval` would otherwise overlap the one it scheduled, on
/// another process, and the two would update the same rows at once.
const REAP_LOCK: i64 = 0x6167_7261_6665; // "agrafe"

/// Enqueue a reap at `run_at`, unless one is already pending.
pub async fn schedule(exec: impl PgExecutor<'_>, run_at: Timestamp) -> sqlx::Result<bool> {
    super::enqueue(
        exec,
        JobKind::Reap,
        None,
        Priority::Normal,
        serde_json::json!({}),
        run_at,
    )
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
    pub purges: usize,
    /// Stale uploads handed back to `created`.
    pub reset: usize,
    /// Dead jobs removed.
    pub pruned: u64,
    /// Rows of purged blobs removed, their attachments and jobs with them.
    pub tombstones: u64,
    /// Another reap was running: this one did nothing.
    pub skipped: bool,
}

/// One pass, in one transaction, as of `now`; or nothing, when another
/// reap holds the lock.
/// Tombstones pruned per reap. The prune runs in the reap's transaction,
/// cascading to attachments and jobs, so a backlog (the first run on an
/// old table, or after an outage) drains at this rate rather than in one
/// transaction: ten thousand a minute is over fourteen million a day.
pub(crate) const TOMBSTONE_BATCH: i64 = 10_000;

pub async fn reap(state: &Agrafe, now: Timestamp) -> Result<Report, JobError> {
    let config = state.config();
    let mut tx = state.pool().begin().await?;

    let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(REAP_LOCK)
        .fetch_one(&mut *tx)
        .await?;
    if !held {
        return Ok(Report {
            skipped: true,
            ..Report::default()
        });
    }

    let expired = repo::expire(&mut tx, now).await?;

    // An export whose backend stopped sending parts, or an attached blob
    // whose file never came; the purge below takes any parts with the
    // prefix.
    let abandoned_before = now
        .checked_sub(config.upload.abandoned_after)
        .unwrap_or(Timestamp::MIN);
    let abandoned = repo::expire_abandoned(&mut tx, abandoned_before).await?;

    // The backend learns of each expiry from here, so `expired` lags the
    // moment by at most one interval.
    if config.webhook.wants(Event::Expired) {
        let ids: Vec<Uuid> = expired.iter().chain(&abandoned).copied().collect();
        notify::enqueue_each(&mut *tx, &ids, Event::Expired, now).await?;
    }

    // Dead jobs go first, so a purge that died is scheduled again below,
    // in the same pass, once it has been around long enough.
    let dead_before = now
        .checked_sub(config.jobs.keep_failed)
        .unwrap_or(Timestamp::MIN);
    let pruned = super::prune_failed(&mut *tx, dead_before).await?;

    // Everything terminal still holding bytes and not already on its way
    // out: the blobs just expired, infected ones past their expiry, and
    // any whose purge was lost.
    let purge_at = config.upload.purge_at(now);
    let mut purges = 0;
    for id in repo::awaiting_purge(&mut tx, now).await? {
        if super::enqueue(
            &mut *tx,
            JobKind::Purge,
            Some(id),
            Priority::Normal,
            serde_json::json!({}),
            purge_at,
        )
        .await?
        {
            purges += 1;
        }
    }

    let stale_before = now
        .checked_sub(config.upload.stale_after)
        .unwrap_or(Timestamp::MIN);
    let reset = repo::reset_stale_uploads(&mut tx, stale_before).await?;

    // Tombstones that have answered `GET /blobs/:id` long enough.
    let tombstone_before = now
        .checked_sub(config.blobs.keep_purged)
        .unwrap_or(Timestamp::MIN);
    let tombstones = repo::prune_tombstones(&mut tx, tombstone_before, TOMBSTONE_BATCH).await?;

    tx.commit().await?;
    Ok(Report {
        expired: expired.len(),
        abandoned: abandoned.len(),
        purges,
        reset: reset.len(),
        pruned,
        tombstones,
        skipped: false,
    })
}
