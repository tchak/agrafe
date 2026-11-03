//! Workers: a fixed number of pollers per job kind, each claiming and running
//! one job at a time. They sleep on `poll_interval` and wake early on the
//! `NOTIFY` every enqueue sends.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use jiff::Timestamp;
use sqlx::postgres::PgListener;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tracing::Instrument as _;
use uuid::Uuid;

use super::{
    Failure, Job, JobError, JobKind, archive, audit, document, fetch, notify, purge, reap, render,
    rows, scan,
};
use crate::app::Agrafe;
use crate::metrics::Outcome;
use crate::trace::TraceContext;

/// Pause after a listener error before asking Postgres again.
const LISTEN_RETRY: Duration = Duration::from_secs(1);

/// Why the workers stopped. A failed job is not one of these.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The connection for `LISTEN` could not be opened.
    #[error("connecting the job listener: {0}")]
    Connect(sqlx::Error),
    /// Subscribing to the job channel failed.
    #[error("listening for jobs: {0}")]
    Listen(sqlx::Error),
    /// A poller or the listener panicked; a handler's panic is a failed
    /// job instead.
    #[error("worker panicked: {0}")]
    Panicked(#[from] tokio::task::JoinError),
    /// The sandbox, renderer or document compiler probe failed, for a
    /// process handling `render` or `document`: the tools are not fit to
    /// run.
    #[error("the renderer failed its startup probe: {0}")]
    Probe(#[from] agrafe_render::ProbeError),
    /// The deployment offers DOCX and ODT documents and this process
    /// handles `document` jobs with no pandoc to write them.
    #[error(
        "document.office_formats is on and this worker handles document jobs, \
         but media.pandoc_command is not set"
    )]
    NoPandoc,
    /// The first reap or audit could not be enqueued, for a process
    /// handling `reap` or `audit`.
    #[error("scheduling the periodic jobs: {0}")]
    Schedule(sqlx::Error),
}

/// Run the workers until `shutdown` resolves. A job in flight finishes;
/// nothing new is claimed afterwards.
pub async fn run<F>(state: Agrafe, shutdown: F) -> Result<(), Error>
where
    F: Future<Output = ()> + Clone + Send + 'static,
{
    let handled = state.config().jobs.kinds();
    if handled.contains(&JobKind::Document)
        && state.config().document.office_formats
        && state.config().media.render.pandoc_command.is_none()
    {
        return Err(Error::NoPandoc);
    }
    if handled.contains(&JobKind::Render) || handled.contains(&JobKind::Document) {
        // What confines the tools, checked before any of them runs: a
        // kernel that cannot refuse is a startup error under `required`,
        // and a log line otherwise.
        let sandbox = agrafe_render::probe_sandbox(&state.config().media.render.sandbox).await?;
        tracing::info!("sandbox: {sandbox}");
        // The office converter is only as contained as Landlock makes it,
        // so a host without it, or with the work directories where the
        // converter may remove files, does not get to run it.
        let media = &state.config().media;
        agrafe_render::check_office_host(&media.render, media.tmp_dir.as_deref(), &sandbox)?;
    }
    if handled.contains(&JobKind::Render) {
        let media = &state.config().media;
        agrafe_render::probe(&media.render, media.tmp_dir.as_deref()).await?;
        tracing::info!("renderer probe passed");
    }
    if handled.contains(&JobKind::Document) {
        // Templates are written against a compiler version; a helper that
        // is not the compiler, or cannot read its workspace, must fail the
        // deployment, not every document.
        let media = &state.config().media;
        agrafe_render::probe_document(&media.render, media.tmp_dir.as_deref()).await?;
        tracing::info!("document compiler probe passed");
        if media.render.pandoc_command.is_none() {
            tracing::info!("no pandoc configured, documents build to PDF only");
        }
    }
    if handled.contains(&JobKind::Reap) {
        // The reaper reschedules itself from then on.
        reap::schedule(state.pool(), Timestamp::now())
            .await
            .map_err(Error::Schedule)?;
    }
    if handled.contains(&JobKind::Audit) {
        // One interval out, not now: a sweep of the bucket on every
        // restart is not wanted, and a pending one survives the restart
        // anyway, the duplicate dropped.
        audit::schedule(
            state.pool(),
            Timestamp::now() + state.config().jobs.audit_interval,
        )
        .await
        .map_err(Error::Schedule)?;
    }

    let wakeups: HashMap<JobKind, Arc<Notify>> = handled
        .iter()
        .map(|kind| (*kind, Arc::new(Notify::new())))
        .collect();
    let mut tasks = JoinSet::new();

    let listener = PgListener::connect_with(state.pool())
        .await
        .map_err(Error::Connect)?;
    tasks.spawn(listen(listener, wakeups.clone(), shutdown.clone()));

    let worker = format!("{}-{}", hostname(), Uuid::now_v7());
    for kind in &handled {
        let count = state.config().jobs.concurrency_for(*kind);
        state.metrics().set_workers(*kind, count);
        tracing::info!(kind = %kind, workers = count, "starting workers");
        for n in 0..count {
            tasks.spawn(poll(
                state.clone(),
                *kind,
                format!("{worker}/{kind}/{n}"),
                wakeups[kind].clone(),
                shutdown.clone(),
            ));
        }
    }

    while let Some(result) = tasks.join_next().await {
        result??;
    }
    Ok(())
}

/// Claim and run one job of `kind`, to the end. Returns whether there was
/// one. This is one turn of a worker, and what tests call.
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub async fn run_one(state: &Agrafe, kind: JobKind, worker: &str) -> sqlx::Result<bool> {
    turn(state, kind, worker, std::future::pending()).await
}

/// One turn of a worker: claim and run one job of `kind`, or hand it back
/// to the queue once `shutdown` resolves and `jobs.shutdown_grace` has
/// passed without the job finishing.
async fn turn<F>(state: &Agrafe, kind: JobKind, worker: &str, shutdown: F) -> sqlx::Result<bool>
where
    F: Future<Output = ()>,
{
    let lease = state.config().jobs.lease_timeout;
    let Some(job) = super::claim(state.pool(), kind, worker, lease).await? else {
        return Ok(false);
    };
    // Each attempt is a unit of the trace that enqueued the job, or the
    // first of its own for a job nothing traced (the periodic ones): the
    // jobs it enqueues in turn join that trace.
    let span = tracing::info_span!(
        "job",
        id = job.id,
        kind = %job.kind,
        blob = ?job.blob_id,
        attempt = job.attempts,
        trace_id = tracing::field::Empty,
    );
    let context = TraceContext::start(&span, job.trace_context());
    // The span is attached to the future rather than entered on the
    // thread: a guard held across an await would tag whatever else the
    // thread polls meanwhile.
    context
        .scope(run_claimed(state, &job, worker, shutdown).instrument(span))
        .await?;
    Ok(true)
}

async fn run_claimed<F>(state: &Agrafe, job: &Job, worker: &str, shutdown: F) -> sqlx::Result<()>
where
    F: Future<Output = ()>,
{
    let lease = state.config().jobs.lease_timeout;
    let grace = state.config().jobs.shutdown_grace;
    let started = Instant::now();
    // A handler that panics has failed this attempt; it must not take the
    // worker, or the process in the `all` role, down with it. The lease is
    // renewed while it runs, so a job longer than the lease is not handed
    // to a second worker. A lease lost all the same, another worker holding
    // the job, ends the attempt where it stands: two runs of one job would
    // write the same objects, and the row is the other worker's to finish.
    // A process stopping gives the job its grace, then hands it back the
    // same way, for another process to take now rather than after the
    // lease lapses under the supervisor's kill.
    let outcome = tokio::select! {
        outcome = std::panic::AssertUnwindSafe(handle(state, job)).catch_unwind() => {
            outcome.unwrap_or_else(|panic| {
                Err(JobError::Panicked(panic_message(&*panic).to_string()))
            })
        }
        () = heartbeat(state, job.id, worker, lease) => {
            tracing::warn!("lease lost: the job was reclaimed or removed, attempt abandoned");
            return Ok(());
        }
        () = grace_expired(shutdown, grace) => {
            let mut conn = state.pool().acquire().await?;
            let outcome = match super::release(&mut conn, job, Timestamp::now()).await? {
                Failure::Released => {
                    tracing::info!("shutting down: job handed back to the queue");
                    Outcome::Released
                }
                Failure::Superseded => {
                    tracing::info!("shutting down: job dropped, a newer twin runs instead");
                    Outcome::Superseded
                }
                Failure::Retry(_) | Failure::Dead | Failure::Lost => {
                    tracing::warn!("lease lost: the job was reclaimed or removed, row left alone");
                    return Ok(());
                }
            };
            state.metrics().job_run(job.kind, outcome, started.elapsed());
            return Ok(());
        }
    };
    let took = started.elapsed();
    let outcome = match outcome {
        Ok(()) => {
            let removed = settle(|| async move {
                let mut conn = state.pool().acquire().await?;
                super::succeed(&mut conn, job).await
            })
            .await?;
            if !removed {
                tracing::warn!("lease lost: the job was reclaimed or removed, row left alone");
                return Ok(());
            }
            tracing::info!(took_ms = took.as_millis() as u64, "job done");
            Outcome::Ok
        }
        Err(err) => {
            let error = err.to_string();
            let permanent = err.is_permanent();
            let retry_after = err.retry_after();
            let error_ref = &error;
            let failure = settle(|| async move {
                // One transaction: a death stamped on the blob commits with
                // the notification of it.
                let mut tx = state.pool().begin().await?;
                let failure = super::fail(
                    &mut tx,
                    job,
                    error_ref,
                    permanent,
                    retry_after,
                    &state.config().webhook,
                    Timestamp::now(),
                )
                .await?;
                tx.commit().await?;
                Ok(failure)
            })
            .await?;
            // As `last_error`, the column's name, and for what the exporters
            // key on: it may quote what a user's file or a host's manifest
            // says, and stays in the logs and the row. See DESIGN.md, "Tracing".
            match failure {
                Failure::Retry(at) => {
                    tracing::warn!(last_error = error, retry_at = %at, "job failed");
                    Outcome::Retry
                }
                Failure::Dead => {
                    tracing::error!(
                        last_error = error,
                        tags.job_kind = %job.kind,
                        "job failed for the last time"
                    );
                    Outcome::Dead
                }
                Failure::Superseded => {
                    tracing::warn!(last_error = error, "job failed and was superseded");
                    Outcome::Superseded
                }
                // `fail` never releases; the arm is for the enum's sake.
                Failure::Lost | Failure::Released => {
                    tracing::warn!(
                        last_error = error,
                        "job failed after its lease was lost: the job was reclaimed or removed, row left alone"
                    );
                    return Ok(());
                }
            }
        }
    };
    state.metrics().job_run(job.kind, outcome, took);
    Ok(())
}

async fn handle(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    match job.kind {
        JobKind::Fetch => fetch::run(state, job).await,
        JobKind::Scan => scan::run(state, job).await,
        JobKind::Render => render::run(state, job).await,
        JobKind::Rows => rows::run(state, job).await,
        JobKind::Archive => archive::run(state, job).await,
        JobKind::Document => document::run(state, job).await,
        JobKind::Purge => purge::run(state, job).await,
        JobKind::Reap => reap::run(state, job).await,
        JobKind::Notify => notify::run(state, job).await,
        JobKind::Audit => audit::run(state, job).await,
    }
}

async fn poll<F>(
    state: Agrafe,
    kind: JobKind,
    worker: String,
    wakeup: Arc<Notify>,
    shutdown: F,
) -> Result<(), Error>
where
    F: Future<Output = ()> + Clone,
{
    let interval = state.config().jobs.poll_interval;
    let signal = shutdown.clone();
    tokio::pin!(shutdown);
    loop {
        // Nothing is claimed once the signal came, however much is due: a
        // backlog would otherwise hold the process past its grace period.
        if (&mut shutdown).now_or_never().is_some() {
            return Ok(());
        }
        // Drain what is due, then wait. The wakeup is registered before the
        // claim: `notify_waiters` wakes only the futures already enabled,
        // and an enqueue that commits while the claim runs, after its
        // snapshot, would otherwise be missed and cost one interval. A
        // wakeup that lands between a claim that found a job and the next
        // claim is moot, since the next claim runs anyway.
        let notified = wakeup.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let wait = match turn(&state, kind, &worker, signal.clone()).await {
            Ok(true) => continue,
            Ok(false) => interval,
            Err(err) => {
                tracing::error!(kind = %kind, error = %err, "worker turn failed");
                interval
            }
        };
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            _ = &mut notified => {}
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

async fn listen<F>(
    mut listener: PgListener,
    wakeups: HashMap<JobKind, Arc<Notify>>,
    shutdown: F,
) -> Result<(), Error>
where
    F: Future<Output = ()>,
{
    listener
        .listen(super::CHANNEL)
        .await
        .map_err(Error::Listen)?;
    tokio::pin!(shutdown);
    loop {
        // `recv` reconnects by itself after a dropped connection; the
        // pollers cover anything missed meanwhile.
        let notification = tokio::select! {
            _ = &mut shutdown => return Ok(()),
            n = listener.recv() => n,
        };
        match notification {
            Ok(n) => match n.payload().parse::<JobKind>() {
                Ok(kind) => {
                    if let Some(wakeup) = wakeups.get(&kind) {
                        wakeup.notify_waiters();
                    }
                }
                Err(err) => tracing::warn!(error = %err, "ignoring job notification"),
            },
            Err(err) => {
                tracing::warn!(error = ?err, "job listener error");
                // `recv` tries to reconnect on the next call; while the
                // database is down that would be a hot loop of failed
                // connects. The pollers keep draining the table meanwhile.
                tokio::select! {
                    _ = &mut shutdown => return Ok(()),
                    _ = tokio::time::sleep(LISTEN_RETRY) => {}
                }
            }
        }
    }
}

/// Pauses between the tries of an end-of-attempt write: half a second,
/// doubling, so the last try comes a few seconds after the first, well
/// inside the lease.
const SETTLE_RETRIES: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

/// Run the write that ends an attempt, again after a transient database
/// error. The work is done by then, and a write that does not land leaves
/// the row locked with it: the job would run again once the lease lapsed,
/// a delivery twice, a render for nothing, and lose an attempt to a
/// connection reset. The write is conditioned on holding the row, so a
/// try that landed before the error reached us is harmless to repeat.
async fn settle<T, F, Fut>(write: F) -> sqlx::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = sqlx::Result<T>>,
{
    settle_with(&SETTLE_RETRIES, write).await
}

/// `settle`, pausing for each of `pauses` in turn between the tries.
async fn settle_with<T, F, Fut>(pauses: &[Duration], mut write: F) -> sqlx::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = sqlx::Result<T>>,
{
    let mut pauses = pauses.iter();
    loop {
        match write().await {
            Err(err) if is_transient(&err) => match pauses.next() {
                Some(pause) => {
                    tracing::warn!(error = %err, "could not record the attempt's end, trying again");
                    tokio::time::sleep(*pause).await;
                }
                None => return Err(err),
            },
            result => return result,
        }
    }
}

/// Whether a database error is the connection's or the server's moment
/// rather than the statement's: a lost or refused connection, a pool with
/// nothing to give, a transaction the server rolled back (a deadlock, a
/// serialization failure), a server shutting down or out of resources.
/// Repeating those is worth a try; a statement the server refused fails
/// the same way again.
fn is_transient(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::Protocol(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::WorkerCrashed => true,
        sqlx::Error::Database(db) => db.code().is_some_and(|code| {
            // SQLSTATE classes: 08 connection exception, 40 transaction
            // rollback, 53 insufficient resources, 57 operator intervention.
            ["08", "40", "53", "57"]
                .iter()
                .any(|class| code.starts_with(class))
        }),
        _ => false,
    }
}

/// Resolves `grace` after `shutdown` does: the time a stopping process
/// gives the job it holds before handing it back.
async fn grace_expired<F: Future<Output = ()>>(shutdown: F, grace: Duration) {
    shutdown.await;
    tokio::time::sleep(grace).await;
}

/// Renew `worker`'s lease on the job until it is lost: another worker
/// claimed the job after a lapse, or the job is gone. A renewal that fails
/// is retried at the next tick; the lease may still hold.
async fn heartbeat(state: &Agrafe, id: i64, worker: &str, lease: Duration) {
    let period = (lease / 3).max(Duration::from_secs(1));
    let mut ticks = tokio::time::interval(period);
    ticks.tick().await; // the first tick is immediate; the claim just set the lease
    loop {
        ticks.tick().await;
        match super::touch(state.pool(), id, worker).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(err) => tracing::warn!(error = ?err, "could not renew the job lease"),
        }
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| std::process::id().to_string())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const NO_PAUSE: [Duration; 3] = [Duration::ZERO; 3];

    fn transient() -> sqlx::Error {
        sqlx::Error::PoolTimedOut
    }

    #[tokio::test]
    async fn settle_tries_again_after_a_transient_error() {
        let tries = Cell::new(0);
        let result = settle_with(&NO_PAUSE, || async {
            tries.set(tries.get() + 1);
            if tries.get() < 3 {
                Err(transient())
            } else {
                Ok(tries.get())
            }
        })
        .await;
        assert_eq!(result.unwrap(), 3);
    }

    #[tokio::test]
    async fn settle_gives_up_after_its_retries() {
        let tries = Cell::new(0);
        let result = settle_with(&NO_PAUSE, || async {
            tries.set(tries.get() + 1);
            Err::<(), _>(transient())
        })
        .await;
        assert!(matches!(result, Err(sqlx::Error::PoolTimedOut)));
        assert_eq!(tries.get(), 1 + NO_PAUSE.len());
    }

    #[tokio::test]
    async fn settle_returns_a_statement_error_at_once() {
        let tries = Cell::new(0);
        let result = settle_with(&NO_PAUSE, || async {
            tries.set(tries.get() + 1);
            Err::<(), _>(sqlx::Error::RowNotFound)
        })
        .await;
        assert!(matches!(result, Err(sqlx::Error::RowNotFound)));
        assert_eq!(tries.get(), 1);
    }
}
