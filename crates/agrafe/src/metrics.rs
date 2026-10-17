//! Prometheus metrics. Queue depth and age are read from the `jobs` table
//! when scraped, so any process answers for the whole deployment; job
//! outcomes and durations are counted by the process that ran them.

use std::time::Duration;

use prometheus::{
    Encoder, GaugeVec, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry,
    TextEncoder,
};
use sqlx::PgPool;

use crate::jobs::{self, JobKind, JobState};

/// What a scrape can fail on.
#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    #[error("counting jobs: {0}")]
    Db(#[from] sqlx::Error),
    #[error("encoding metrics: {0}")]
    Encode(#[from] prometheus::Error),
}

/// Outcome label of a job run.
#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    Ok,
    Retry,
    Dead,
    Superseded,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Retry => "retry",
            Outcome::Dead => "dead",
            Outcome::Superseded => "superseded",
        }
    }
}

/// Seconds. Renders take a few, scans wait on clamd, builds run for minutes.
const DURATION_BUCKETS: &[f64] = &[
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
];

pub struct Metrics {
    registry: Registry,
    job_runs: IntCounterVec,
    job_duration: HistogramVec,
    workers: IntGaugeVec,
    jobs: IntGaugeVec,
    oldest_due: GaugeVec,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let job_runs = IntCounterVec::new(
            Opts::new(
                "agrafe_job_runs_total",
                "Job attempts finished by this process, by kind and outcome",
            ),
            &["kind", "outcome"],
        )
        .expect("valid metric");
        let job_duration = HistogramVec::new(
            HistogramOpts::new(
                "agrafe_job_duration_seconds",
                "Wall time of job attempts run by this process",
            )
            .buckets(DURATION_BUCKETS.to_vec()),
            &["kind"],
        )
        .expect("valid metric");
        let workers = IntGaugeVec::new(
            Opts::new("agrafe_workers", "Workers this process runs, by kind"),
            &["kind"],
        )
        .expect("valid metric");
        let jobs = IntGaugeVec::new(
            Opts::new(
                "agrafe_jobs",
                "Jobs in the queue, by kind and state (due, scheduled, running, dead)",
            ),
            &["kind", "state"],
        )
        .expect("valid metric");
        let oldest_due = GaugeVec::new(
            Opts::new(
                "agrafe_jobs_oldest_due_seconds",
                "How long the oldest unclaimed due job of each kind has been waiting",
            ),
            &["kind"],
        )
        .expect("valid metric");
        let collectors: [Box<dyn prometheus::core::Collector>; 5] = [
            Box::new(job_runs.clone()),
            Box::new(job_duration.clone()),
            Box::new(workers.clone()),
            Box::new(jobs.clone()),
            Box::new(oldest_due.clone()),
        ];
        for c in collectors {
            registry.register(c).expect("metric names are unique");
        }
        // Every outcome series exists from the start, so a rate over a
        // counter that has not moved yet is zero rather than absent.
        for kind in JobKind::ALL {
            for outcome in [
                Outcome::Ok,
                Outcome::Retry,
                Outcome::Dead,
                Outcome::Superseded,
            ] {
                job_runs.with_label_values(&[kind.as_str(), outcome.as_str()]);
            }
        }
        Self {
            registry,
            job_runs,
            job_duration,
            workers,
            jobs,
            oldest_due,
        }
    }

    /// An attempt at a job of `kind` finished with `outcome` after `took`.
    pub fn job_run(&self, kind: JobKind, outcome: Outcome, took: Duration) {
        self.job_runs
            .with_label_values(&[kind.as_str(), outcome.as_str()])
            .inc();
        self.job_duration
            .with_label_values(&[kind.as_str()])
            .observe(took.as_secs_f64());
    }

    pub fn set_workers(&self, kind: JobKind, count: usize) {
        self.workers
            .with_label_values(&[kind.as_str()])
            .set(count as i64);
    }

    /// Refresh the queue gauges from the database and render everything in
    /// the Prometheus text format.
    pub async fn render(&self, pool: &PgPool, lease: Duration) -> Result<String, MetricsError> {
        for kind in JobKind::ALL {
            for state in JobState::ALL {
                self.jobs
                    .with_label_values(&[kind.as_str(), state.as_str()])
                    .set(0);
            }
            self.oldest_due.with_label_values(&[kind.as_str()]).set(0.0);
        }
        for row in jobs::counts(pool, lease).await? {
            self.jobs
                .with_label_values(&[row.kind.as_str(), row.state.as_str()])
                .set(row.count);
            if row.state == JobState::Due {
                self.oldest_due
                    .with_label_values(&[row.kind.as_str()])
                    .set(row.oldest_seconds.unwrap_or(0.0).max(0.0));
            }
        }
        let mut out = Vec::new();
        TextEncoder::new().encode(&self.registry.gather(), &mut out)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}
