//! Workers: a fixed number of pollers per job kind, each claiming and running
//! one job at a time. They sleep on `poll_interval` and wake early on the
//! `NOTIFY` every enqueue sends.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use futures::FutureExt as _;
use jiff::Timestamp;
use sqlx::postgres::PgListener;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tracing::Instrument as _;
use uuid::Uuid;

use super::{Failure, Job, JobKind, scan};
use crate::app::AppState;

/// Kinds this binary knows how to run. Others wait in the table.
pub const HANDLED: &[JobKind] = &[JobKind::Scan];

/// Pause after a listener error before asking Postgres again.
const LISTEN_RETRY: Duration = Duration::from_secs(1);

/// Run the workers until `shutdown` resolves. A job in flight finishes;
/// nothing new is claimed afterwards.
pub async fn run<F>(state: AppState, shutdown: F) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Clone + Send + 'static,
{
    let wakeups: HashMap<JobKind, Arc<Notify>> = HANDLED
        .iter()
        .map(|kind| (*kind, Arc::new(Notify::new())))
        .collect();
    let mut tasks = JoinSet::new();

    let listener = PgListener::connect_with(state.pool())
        .await
        .context("connecting the job listener")?;
    tasks.spawn(listen(listener, wakeups.clone(), shutdown.clone()));

    let worker = format!("{}-{}", hostname(), Uuid::now_v7());
    for kind in HANDLED {
        let count = state.config().jobs.concurrency_for(*kind);
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
        result.context("worker panicked")??;
    }
    Ok(())
}

/// Claim and run one job of `kind`. Returns whether there was one. This is
/// one turn of a worker, and what tests call.
pub async fn run_one(state: &AppState, kind: JobKind, worker: &str) -> anyhow::Result<bool> {
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
    run_claimed(state, &job).instrument(span).await?;
    Ok(true)
}

async fn run_claimed(state: &AppState, job: &Job) -> anyhow::Result<()> {
    // A handler that panics has failed this attempt; it must not take the
    // worker, or the process in the `all` role, down with it.
    let outcome = std::panic::AssertUnwindSafe(handle(state, job))
        .catch_unwind()
        .await
        .unwrap_or_else(|panic| {
            Err(anyhow::anyhow!(
                "handler panicked: {}",
                panic_message(&*panic)
            ))
        });
    match outcome {
        Ok(()) => {
            super::succeed(state.pool(), job.id).await?;
            tracing::info!("job done");
        }
        Err(err) => {
            let mut conn = state.pool().acquire().await?;
            let error = format!("{err:#}");
            match super::fail(&mut conn, job, &error, Timestamp::now()).await? {
                Failure::Retry(at) => tracing::warn!(error, retry_at = %at, "job failed"),
                Failure::Dead => tracing::error!(error, "job failed for the last time"),
                Failure::Superseded => tracing::warn!(error, "job failed and was superseded"),
            }
        }
    }
    Ok(())
}

async fn handle(state: &AppState, job: &Job) -> anyhow::Result<()> {
    match job.kind {
        JobKind::Scan => scan::run(state, job).await,
        other => anyhow::bail!("no handler for `{other}` jobs"),
    }
}

async fn poll<F>(
    state: AppState,
    kind: JobKind,
    worker: String,
    wakeup: Arc<Notify>,
    shutdown: F,
) -> anyhow::Result<()>
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
                tracing::error!(kind = %kind, error = ?err, "worker turn failed");
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
) -> anyhow::Result<()>
where
    F: Future<Output = ()>,
{
    listener
        .listen(super::CHANNEL)
        .await
        .context("listening for jobs")?;
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
