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
    Failure, Job, JobError, JobKind, archive, document, fetch, notify, purge, reap, render, rows,
    scan,
};
use crate::app::Agrafe;
use crate::metrics::Outcome;

/// Pause after a listener error before asking Postgres again.
const LISTEN_RETRY: Duration = Duration::from_secs(1);

/// Why the workers stopped. A failed job is not one of these.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("connecting the job listener: {0}")]
    Connect(sqlx::Error),
    #[error("listening for jobs: {0}")]
    Listen(sqlx::Error),
    #[error("worker panicked: {0}")]
    Panicked(#[from] tokio::task::JoinError),
    #[error("the renderer failed its startup probe: {0}")]
    Probe(#[from] agrafe_render::ProbeError),
    #[error("scheduling the first reap: {0}")]
    Schedule(sqlx::Error),
}

/// Run the workers until `shutdown` resolves. A job in flight finishes;
/// nothing new is claimed afterwards.
pub async fn run<F>(state: Agrafe, shutdown: F) -> Result<(), Error>
where
    F: Future<Output = ()> + Clone + Send + 'static,
{
    let handled = state.config().jobs.kinds();
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
    }
    if handled.contains(&JobKind::Reap) {
        // The reaper reschedules itself from then on.
        reap::schedule(state.pool(), Timestamp::now())
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

/// Claim and run one job of `kind`. Returns whether there was one. This is
/// one turn of a worker, and what tests call.
pub async fn run_one(state: &Agrafe, kind: JobKind, worker: &str) -> sqlx::Result<bool> {
    let lease = state.config().jobs.lease_timeout;
    let Some(job) = super::claim(state.pool(), &[kind], worker, lease).await? else {
        return Ok(false);
    };
    let span = tracing::info_span!(
        "job",
        id = job.id,
        kind = %job.kind,
        blob = ?job.blob_id,
        attempt = job.attempts
    );
    // The span is attached to the future rather than entered on the
    // thread: a guard held across an await would tag whatever else the
    // thread polls meanwhile.
    run_claimed(state, &job, worker).instrument(span).await?;
    Ok(true)
}

async fn run_claimed(state: &Agrafe, job: &Job, worker: &str) -> sqlx::Result<()> {
    let lease = state.config().jobs.lease_timeout;
    let started = Instant::now();
    // A handler that panics has failed this attempt; it must not take the
    // worker, or the process in the `all` role, down with it. The lease is
    // renewed while it runs, so a job longer than the lease is not handed
    // to a second worker.
    let outcome = tokio::select! {
        outcome = std::panic::AssertUnwindSafe(handle(state, job)).catch_unwind() => {
            outcome.unwrap_or_else(|panic| {
                Err(JobError::Panicked(panic_message(&*panic).to_string()))
            })
        }
        _ = heartbeat(state, job.id, worker, lease) => unreachable!("the heartbeat never ends"),
    };
    let took = started.elapsed();
    let outcome = match outcome {
        Ok(()) => {
            let mut conn = state.pool().acquire().await?;
            super::succeed(&mut conn, job).await?;
            tracing::info!(took_ms = took.as_millis() as u64, "job done");
            Outcome::Ok
        }
        Err(err) => {
            // One transaction: a death stamped on the blob commits with
            // the notification of it.
            let mut tx = state.pool().begin().await?;
            let error = err.to_string();
            let permanent = err.is_permanent();
            let retry_after = err.retry_after();
            let failure = super::fail(
                &mut tx,
                job,
                &error,
                permanent,
                retry_after,
                &state.config().webhook,
                Timestamp::now(),
            )
            .await?;
            tx.commit().await?;
            match failure {
                Failure::Retry(at) => {
                    tracing::warn!(error, retry_at = %at, "job failed");
                    Outcome::Retry
                }
                Failure::Dead => {
                    tracing::error!(error, "job failed for the last time");
                    Outcome::Dead
                }
                Failure::Superseded => {
                    tracing::warn!(error, "job failed and was superseded");
                    Outcome::Superseded
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
    F: Future<Output = ()>,
{
    let interval = state.config().jobs.poll_interval;
    tokio::pin!(shutdown);
    loop {
        // Drain what is due, then wait. The wakeup is best effort: a notify
        // that lands between an empty claim and the wait costs one interval.
        let wait = match run_one(&state, kind, &worker).await {
            Ok(true) => continue,
            Ok(false) => interval,
            Err(err) => {
                tracing::error!(kind = %kind, error = %err, "worker turn failed");
                interval
            }
        };
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            _ = wakeup.notified() => {}
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

/// Renew the lease every third of it, for as long as this future is polled.
async fn heartbeat(state: &Agrafe, id: i64, worker: &str, lease: Duration) {
    let period = (lease / 3).max(Duration::from_secs(1));
    let mut ticks = tokio::time::interval(period);
    ticks.tick().await; // the first tick is immediate; the claim just set the lease
    loop {
        ticks.tick().await;
        match super::touch(state.pool(), id, worker).await {
            Ok(true) => {}
            Ok(false) => tracing::warn!("lease lost: the job was reclaimed or removed"),
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
