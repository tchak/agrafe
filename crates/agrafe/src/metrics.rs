//! Prometheus metrics. Queue depth and age come from the census of the
//! `jobs` table taken when scraped (`Agrafe::job_counts`), so any process
//! answers for the whole deployment; job outcomes and durations are
//! counted by the process that ran them.

use std::time::Duration;

use jiff::Timestamp;
use prometheus::{
    Encoder, GaugeVec, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder,
};

use crate::jobs::audit::{Finding, Report};
use crate::jobs::notify::Event;
use crate::jobs::{Count, JobKind, JobState, Priority};

/// What a scrape can fail on.
#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    /// The registry could not be written out in the text format.
    #[error("encoding metrics: {0}")]
    Encode(#[from] prometheus::Error),
}

/// Outcome label of a job run.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Outcome {
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

/// The registry of this process's Prometheus metrics: job runs and
/// durations, workers, the queue census, the read cache, webhook
/// deliveries and the last audit. Rendered by `render` for `/metrics`.
pub struct Metrics {
    registry: Registry,
    job_runs: IntCounterVec,
    job_duration: HistogramVec,
    workers: IntGaugeVec,
    jobs: IntGaugeVec,
    oldest_due: GaugeVec,
    head_cache: IntCounterVec,
    webhook_deliveries: IntCounterVec,
    audit_findings: IntGaugeVec,
    audit_objects: IntGauge,
    audit_blobs: IntGauge,
    audit_last_run: IntGauge,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// A registry with every metric registered and at zero.
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
                "Jobs in the queue, by kind, priority and state (due, scheduled, running, dead)",
            ),
            &["kind", "state", "priority"],
        )
        .expect("valid metric");
        let oldest_due = GaugeVec::new(
            Opts::new(
                "agrafe_jobs_oldest_due_seconds",
                "How long the oldest unclaimed due job of each kind and priority has been waiting",
            ),
            &["kind", "priority"],
        )
        .expect("valid metric");
        let head_cache = IntCounterVec::new(
            Opts::new(
                "agrafe_blob_head_cache_total",
                "Row lookups by the read routes, by whether the cache had the row",
            ),
            &["result"],
        )
        .expect("valid metric");
        let webhook_deliveries = IntCounterVec::new(
            Opts::new(
                "agrafe_webhook_deliveries_total",
                "Webhook deliveries attempted by this process, by event and result",
            ),
            &["event", "result"],
        )
        .expect("valid metric");
        let audit_findings = IntGaugeVec::new(
            Opts::new(
                "agrafe_audit_findings",
                "Mismatches between the bucket and the blobs table found by the last audit this process ran, by kind",
            ),
            &["kind"],
        )
        .expect("valid metric");
        let audit_objects = IntGauge::new(
            "agrafe_audit_objects",
            "Objects the last audit this process ran listed in the bucket",
        )
        .expect("valid metric");
        let audit_blobs = IntGauge::new(
            "agrafe_audit_blobs",
            "Blobs the last audit this process ran checked",
        )
        .expect("valid metric");
        let audit_last_run = IntGauge::new(
            "agrafe_audit_last_run_timestamp_seconds",
            "When the last audit this process ran finished, as a Unix time; zero until one has",
        )
        .expect("valid metric");
        let collectors: [Box<dyn prometheus::core::Collector>; 11] = [
            Box::new(job_runs.clone()),
            Box::new(job_duration.clone()),
            Box::new(workers.clone()),
            Box::new(jobs.clone()),
            Box::new(oldest_due.clone()),
            Box::new(head_cache.clone()),
            Box::new(webhook_deliveries.clone()),
            Box::new(audit_findings.clone()),
            Box::new(audit_objects.clone()),
            Box::new(audit_blobs.clone()),
            Box::new(audit_last_run.clone()),
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
        for result in ["hit", "miss"] {
            head_cache.with_label_values(&[result]);
        }
        for event in Event::ALL {
            for result in ["delivered", "failed"] {
                webhook_deliveries.with_label_values(&[event.as_str(), result]);
            }
        }
        for finding in Finding::ALL {
            audit_findings.with_label_values(&[finding.as_str()]);
        }
        Self {
            registry,
            job_runs,
            job_duration,
            workers,
            jobs,
            oldest_due,
            head_cache,
            webhook_deliveries,
            audit_findings,
            audit_objects,
            audit_blobs,
            audit_last_run,
        }
    }

    /// An audit finished at `at` with `report`: what it saw and found, as
    /// of the last run, for the alert on any finding at all.
    pub(crate) fn audit(&self, report: &Report, at: Timestamp) {
        for finding in Finding::ALL {
            self.audit_findings
                .with_label_values(&[finding.as_str()])
                .set(report.count(*finding) as i64);
        }
        self.audit_objects.set(report.objects as i64);
        self.audit_blobs.set(report.blobs as i64);
        self.audit_last_run.set(at.as_second());
    }

    /// A read route looked a row up, and the cache had it or not.
    pub(crate) fn head_cache(&self, hit: bool) {
        self.head_cache
            .with_label_values(&[if hit { "hit" } else { "miss" }])
            .inc();
    }

    /// A delivery of `event` was attempted and the receiver took it or not.
    pub(crate) fn webhook_delivery(&self, event: Event, delivered: bool) {
        self.webhook_deliveries
            .with_label_values(&[
                event.as_str(),
                if delivered { "delivered" } else { "failed" },
            ])
            .inc();
    }

    /// An attempt at a job of `kind` finished with `outcome` after `took`.
    pub(crate) fn job_run(&self, kind: JobKind, outcome: Outcome, took: Duration) {
        self.job_runs
            .with_label_values(&[kind.as_str(), outcome.as_str()])
            .inc();
        self.job_duration
            .with_label_values(&[kind.as_str()])
            .observe(took.as_secs_f64());
    }

    pub(crate) fn set_workers(&self, kind: JobKind, count: usize) {
        self.workers
            .with_label_values(&[kind.as_str()])
            .set(count as i64);
    }

    /// Refresh the queue gauges from `counts`, the census the caller just
    /// took, and render everything in the Prometheus text format.
    pub fn render(&self, counts: &[Count]) -> Result<String, MetricsError> {
        for kind in JobKind::ALL {
            for priority in Priority::ALL {
                for state in JobState::ALL {
                    self.jobs
                        .with_label_values(&[kind.as_str(), state.as_str(), priority.as_str()])
                        .set(0);
                }
                self.oldest_due
                    .with_label_values(&[kind.as_str(), priority.as_str()])
                    .set(0.0);
            }
        }
        for row in counts {
            let (kind, priority) = (row.kind.as_str(), row.priority.as_str());
            self.jobs
                .with_label_values(&[kind, row.state.as_str(), priority])
                .set(row.count);
            if let Some(seconds) = row.oldest_due_seconds {
                self.oldest_due
                    .with_label_values(&[kind, priority])
                    .set(seconds);
            }
        }
        let mut out = Vec::new();
        TextEncoder::new().encode(&self.registry.gather(), &mut out)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}
