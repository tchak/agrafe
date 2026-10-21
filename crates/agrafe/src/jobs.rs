//! Postgres-backed job queue: the `jobs` table, claimed with
//! `FOR UPDATE SKIP LOCKED` and woken by `NOTIFY`. The workers live in
//! `runner`, the handlers in their own modules.

pub(crate) mod archive;
pub mod audit;
pub(crate) mod build;
pub(crate) mod document;
pub(crate) mod fetch;
pub mod notify;
pub(crate) mod purge;
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub mod reap;
pub(crate) mod render;
pub(crate) mod rows;
pub mod runner;
pub(crate) mod scan;

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use jiff_sqlx::ToSqlx;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgExecutor, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::app::Agrafe;
use crate::config::WebhookConfig;
use crate::trace::TraceContext;

/// Why a job listing was refused.
#[derive(Debug, thiserror::Error)]
pub enum ListError {
    /// `limit` lies outside `1..=MAX_LIMIT`; carries `MAX_LIMIT`.
    #[error("limit must be between 1 and {0}")]
    Limit(usize),
    /// The query failed.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

/// The queue as operators see it. Jobs are listed in any state; only a
/// dead one can be retried or discarded, since a pending job is the only
/// thing that will finish its blob.
impl Agrafe {
    /// List jobs, newest first. `limit` must lie in `1..=MAX_LIMIT`.
    pub async fn list_jobs(&self, params: &ListParams) -> Result<Page, ListError> {
        if !(1..=MAX_LIMIT).contains(&params.limit) {
            return Err(ListError::Limit(MAX_LIMIT));
        }
        Ok(list(self.pool(), params, self.config().jobs.lease_timeout).await?)
    }

    /// The job with this id, in any state, or `None`.
    pub async fn find_job(&self, id: i64) -> sqlx::Result<Option<Job>> {
        find(self.pool(), id).await
    }

    /// The state of `job` now, given this instance's lease timeout.
    pub fn job_state(&self, job: &Job) -> JobState {
        job.state(self.config().jobs.lease_timeout, Timestamp::now())
    }

    /// Put a dead job back in the queue.
    pub async fn retry_job(&self, id: i64) -> sqlx::Result<Action> {
        let mut conn = self.pool().acquire().await?;
        retry(&mut conn, id).await
    }

    /// Remove a dead job.
    pub async fn discard_job(&self, id: i64) -> sqlx::Result<Action> {
        let mut conn = self.pool().acquire().await?;
        discard(&mut conn, id).await
    }

    /// The queue census: how many jobs of each kind and priority are in
    /// each state, and how long the oldest due one has waited. What
    /// `/metrics` and the dashboard both show.
    pub async fn job_counts(&self) -> sqlx::Result<Vec<Count>> {
        counts(self.pool(), self.config().jobs.lease_timeout).await
    }
}

/// Why one attempt at a job failed. The message is what `last_error` keeps.
#[derive(Debug, thiserror::Error)]
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub enum JobError {
    #[error("{0} job has no blob")]
    NoBlob(JobKind),
    #[error("no handler for `{0}` jobs")]
    Unhandled(JobKind),
    #[error("handler panicked: {0}")]
    Panicked(String),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Read(#[from] crate::blob::read::ReadError),
    #[error(transparent)]
    Clamd(#[from] crate::clamav::Error),
    #[error("temporary files: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Render(#[from] agrafe_render::Error),
    #[error(transparent)]
    Representation(#[from] crate::blob::representation::Error),
    #[error("purge: {0}")]
    Purge(#[from] purge::PurgeError),
    #[error("build: {0}")]
    Build(#[from] build::BuildError),
    #[error("fetch: {0}")]
    Fetch(#[from] fetch::FetchError),
    #[error("webhook: {0}")]
    Webhook(#[from] notify::WebhookError),
    #[error("audit: {0}")]
    Audit(#[from] audit::AuditError),
}

impl JobError {
    /// Whether another attempt would fail the same way: the job's own
    /// inputs are wrong, not the world around it. Such a job is marked dead
    /// at once rather than retried through the backoff, so the blob it
    /// leaves behind is visible in the dead jobs now and not an hour on.
    pub(crate) fn is_permanent(&self) -> bool {
        match self {
            JobError::NoBlob(_) | JobError::Unhandled(_) => true,
            JobError::Purge(purge::PurgeError::NotTerminal(_)) => true,
            JobError::Build(err) => err.is_permanent(),
            JobError::Fetch(err) => err.is_permanent(),
            JobError::Webhook(err) => err.is_permanent(),
            _ => false,
        }
    }

    /// A delay the failure asks for before the next attempt, when the
    /// world said so: a remote's `Retry-After`. `fail` waits at least this
    /// long on top of its own backoff.
    pub(crate) fn retry_after(&self) -> Option<Duration> {
        match self {
            JobError::Fetch(err) => err.retry_after(),
            _ => None,
        }
    }
}

/// Channel every enqueue notifies; the payload is the job kind.
pub(crate) const CHANNEL: &str = "agrafe_jobs";

/// Delay before the first retry; doubles with every failure.
const BACKOFF_BASE: Duration = Duration::from_secs(30);
const BACKOFF_MAX: Duration = Duration::from_secs(3600);

/// What a job does. Each kind has its own workers, so a backlog of one
/// never holds up another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Download the bytes of a `url` blob.
    Fetch,
    /// Scan an uploaded blob with clamd.
    Scan,
    /// Thumbnail and preview, whichever is pending.
    Render,
    /// Build a `rows` blob from its stored source.
    Rows,
    /// Build an `archive` blob from its stored source.
    Archive,
    /// Build a `document` blob from its stored source.
    Document,
    /// Delete the objects under a terminal blob's prefix.
    Purge,
    /// The periodic housekeeping: expiry, abandoned uploads, pruning, and
    /// the purges they call for. Each run schedules the next.
    Reap,
    /// Deliver one event of a blob to the webhook. See `notify`.
    Notify,
    /// The reconciliation sweep of the bucket against the `blobs` table,
    /// weekly, each run scheduling the next. See `audit`.
    Audit,
}

agrafe_types::str_enum!(JobKind {
    Fetch => "fetch",
    Scan => "scan",
    Render => "render",
    Rows => "rows",
    Archive => "archive",
    Document => "document",
    Purge => "purge",
    Reap => "reap",
    Notify => "notify",
    Audit => "audit",
});

/// Order within a kind's queue. A blob's priority is copied onto its jobs;
/// jobs without a blob run at `Normal`. Kinds, not priorities, keep one
/// sort of work from starving another: a priority only orders its own kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Runs before the `normal` and `low` jobs of its kind.
    High,
    /// The default, and what purges and blob-less jobs always run at.
    #[default]
    Normal,
    /// Runs only when no `high` or `normal` job of its kind is due.
    Low,
}

agrafe_types::str_enum!(Priority {
    High => "high",
    Normal => "normal",
    Low => "low",
});

impl JobKind {
    /// Whether a job of this kind is what moves its blob along: the fetch
    /// that takes a `url` blob out of `created`, the scan or the build that
    /// takes it out of `uploaded`, the render that fills its
    /// representations. Its death is recorded on the blob; a purge, a
    /// reap or an audit is housekeeping, a notify is the news of the blob
    /// and not its progress, and all of those leave the blob's row alone.
    pub(crate) fn finishes_blob(self) -> bool {
        matches!(
            self,
            JobKind::Fetch
                | JobKind::Scan
                | JobKind::Render
                | JobKind::Rows
                | JobKind::Archive
                | JobKind::Document
        )
    }
}

impl Priority {
    /// What the `jobs.priority` column holds; the claim orders on it.
    // Public for the integration tests, not part of the API.
    #[doc(hidden)]
    pub fn rank(self) -> i16 {
        match self {
            Priority::High => 1,
            Priority::Normal => 0,
            Priority::Low => -1,
        }
    }
}

impl TryFrom<i16> for Priority {
    type Error = crate::ParseEnumError;

    fn try_from(rank: i16) -> Result<Self, Self::Error> {
        match rank {
            1 => Ok(Priority::High),
            0 => Ok(Priority::Normal),
            -1 => Ok(Priority::Low),
            other => Err(crate::ParseEnumError {
                kind: "Priority",
                value: other.to_string(),
            }),
        }
    }
}

/// A row of `jobs`. Success deletes the row, so a job that is read is
/// pending, running or dead.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Job {
    /// From a sequence; the listing's order and cursor.
    pub id: i64,
    /// What the job does.
    #[sqlx(try_from = "String")]
    pub kind: JobKind,
    /// The blob it works on; `None` for a `reap`.
    pub blob_id: Option<Uuid>,
    /// Copied from the blob; `normal` for a purge or a blob-less job.
    #[sqlx(try_from = "i16")]
    pub priority: Priority,
    /// Kind-specific arguments: the event for a `notify`, `{}` otherwise.
    pub payload: serde_json::Value,
    /// When the job is due: now, or later for a retry backing off or a
    /// delayed purge.
    #[sqlx(try_from = "jiff_sqlx::Timestamp")]
    pub run_at: Timestamp,
    /// Claims so far; the claim counts the attempt.
    pub attempts: i32,
    /// Attempts before the job is dead.
    pub max_attempts: i32,
    /// When a worker claimed or last renewed its lease; a lock older than
    /// `jobs.lease_timeout` is no lock.
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub locked_at: Option<Timestamp>,
    /// The worker that holds, or last held, the job.
    pub locked_by: Option<String>,
    /// The message of the last failed attempt.
    pub last_error: Option<String>,
    /// Set when the job ran out of attempts, or failed in a way another
    /// attempt would repeat: it is dead and never claimed again.
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub failed_at: Option<Timestamp>,
    /// The `traceparent` of the request or job that enqueued it, which its
    /// runs continue; see `crate::trace`. Written by `enqueue`, so well
    /// formed, and parsed by `trace_context`.
    pub traceparent: Option<String>,
    /// When the job was enqueued.
    #[sqlx(try_from = "jiff_sqlx::Timestamp")]
    pub created_at: Timestamp,
}

/// Where a job stands, as the claim query sees it. A lock older than the
/// lease is no lock: the job is `due` or `scheduled` again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Unclaimed and runnable now.
    Due,
    /// Unclaimed, `run_at` in the future: a retry backing off or a purge
    /// waiting for its delay.
    Scheduled,
    /// Held by a worker whose lease is current.
    Running,
    /// Out of attempts, kept for inspection.
    Dead,
}

agrafe_types::str_enum!(JobState {
    Due => "due",
    Scheduled => "scheduled",
    Running => "running",
    Dead => "dead",
});

impl Job {
    /// The context of the unit that enqueued the job, if one was set.
    pub fn trace_context(&self) -> Option<TraceContext> {
        self.traceparent.as_deref().and_then(TraceContext::parse)
    }

    pub(crate) fn state(&self, lease: Duration, now: Timestamp) -> JobState {
        if self.failed_at.is_some() {
            return JobState::Dead;
        }
        let held = self.locked_at.is_some_and(|at| {
            now.duration_since(at) < SignedDuration::try_from(lease).unwrap_or(SignedDuration::MAX)
        });
        if held {
            JobState::Running
        } else if self.run_at <= now {
            JobState::Due
        } else {
            JobState::Scheduled
        }
    }
}

/// The SQL twin of `Job::state`, with the lease in seconds bound where
/// `push_state` binds it.
const STATE_SQL: (&str, &str) = (
    "CASE WHEN failed_at IS NOT NULL THEN 'dead' \
          WHEN locked_at IS NOT NULL AND locked_at >= now() - make_interval(secs => ",
    ") THEN 'running' \
          WHEN run_at <= now() THEN 'due' \
          ELSE 'scheduled' END",
);

fn push_state(qb: &mut QueryBuilder<Postgres>, lease: Duration) {
    qb.push(STATE_SQL.0)
        .push_bind(lease.as_secs_f64())
        .push(STATE_SQL.1);
}

/// One line of the queue census: how many jobs of `kind` and `priority`
/// are in `state`, and for the due ones how long the oldest has waited.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Count {
    /// The kind counted.
    #[sqlx(try_from = "String")]
    pub kind: JobKind,
    /// The priority counted.
    #[sqlx(try_from = "i16")]
    pub priority: Priority,
    /// The state counted.
    #[sqlx(try_from = "String")]
    pub state: JobState,
    /// How many jobs there are; never zero, an empty combination has no
    /// line.
    pub count: i64,
    /// How long the oldest due job has waited, in seconds since its
    /// `run_at`: `Some` on the `due` line, `None` on every other.
    pub oldest_due_seconds: Option<f64>,
}

/// Count the jobs by kind, priority and state. Combinations with nothing
/// in them have no row.
pub(crate) async fn counts(exec: impl PgExecutor<'_>, lease: Duration) -> sqlx::Result<Vec<Count>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT kind, priority, state, count(*) AS count, \
                CASE WHEN state = 'due' \
                     THEN EXTRACT(EPOCH FROM now() - min(run_at))::float8 \
                END AS oldest_due_seconds \
         FROM (SELECT kind, priority, run_at, ",
    );
    push_state(&mut qb, lease);
    qb.push(" AS state FROM jobs) s GROUP BY kind, priority, state");
    qb.build_query_as().fetch_all(exec).await
}

/// The page size when the request gives none.
pub const DEFAULT_LIMIT: usize = 50;
/// The largest page a listing hands back.
pub const MAX_LIMIT: usize = 200;

/// The filters and page of a job listing; every filter given must match.
#[derive(Debug, Clone, Default)]
pub struct ListParams {
    /// Only jobs of this kind.
    pub kind: Option<JobKind>,
    /// Only jobs in this state, as `Agrafe::job_state` would tell it.
    pub state: Option<JobState>,
    /// Only jobs at this priority.
    pub priority: Option<Priority>,
    /// Only the jobs of this blob.
    pub blob_id: Option<Uuid>,
    /// The `next_cursor` of the previous page: only jobs older than it.
    pub cursor: Option<i64>,
    /// The page size, in `1..=MAX_LIMIT`.
    pub limit: usize,
}

/// A page of a job listing.
#[derive(Debug)]
pub struct Page {
    /// The jobs, newest first.
    pub items: Vec<Job>,
    /// The cursor of the next page; `None` on the last one.
    pub next_cursor: Option<i64>,
}

/// Newest first, keyset paginated on the id.
pub(crate) async fn list(
    exec: impl PgExecutor<'_>,
    params: &ListParams,
    lease: Duration,
) -> sqlx::Result<Page> {
    let limit = params.limit;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT * FROM jobs WHERE true");
    if let Some(kind) = params.kind {
        qb.push(" AND kind = ").push_bind(kind.as_str());
    }
    if let Some(state) = params.state {
        qb.push(" AND ");
        push_state(&mut qb, lease);
        qb.push(" = ").push_bind(state.as_str());
    }
    if let Some(priority) = params.priority {
        qb.push(" AND priority = ").push_bind(priority.rank());
    }
    if let Some(blob_id) = params.blob_id {
        qb.push(" AND blob_id = ").push_bind(blob_id);
    }
    if let Some(cursor) = params.cursor {
        qb.push(" AND id < ").push_bind(cursor);
    }
    qb.push(" ORDER BY id DESC LIMIT ")
        .push_bind(limit as i64 + 1);
    let mut items: Vec<Job> = qb.build_query_as().fetch_all(exec).await?;
    let next_cursor = if items.len() > limit {
        items.truncate(limit);
        items.last().map(|j| j.id)
    } else {
        None
    };
    Ok(Page { items, next_cursor })
}

// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub async fn find(exec: impl PgExecutor<'_>, id: i64) -> sqlx::Result<Option<Job>> {
    sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_optional(exec)
        .await
}

/// Outcome of an operator's action on a job.
#[derive(Debug)]
pub enum Action {
    /// The job was retried or discarded; the row as it was left, or as it
    /// was before the delete.
    Done(Job),
    /// Only a dead job can be acted on; this one is in another state.
    NotDead(Job),
    /// A pending job of the same kind already exists for the blob, so the
    /// dead one has nothing to add.
    Duplicate(Job),
    /// No job has this id.
    Missing,
}

/// Give a dead job its attempts back and run it now. Workers are notified.
pub(crate) async fn retry(conn: &mut PgConnection, id: i64) -> sqlx::Result<Action> {
    let result = sqlx::query_as::<_, Job>(
        "UPDATE jobs SET failed_at = NULL, locked_at = NULL, locked_by = NULL, \
                         attempts = 0, run_at = now() \
         WHERE id = $1 AND failed_at IS NOT NULL RETURNING *",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await;
    match result {
        Ok(Some(job)) => {
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(CHANNEL)
                .bind(job.kind.as_str())
                .execute(&mut *conn)
                .await?;
            if let Some(blob_id) = job.blob_id.filter(|_| job.kind.finishes_blob()) {
                clear_failure(&mut *conn, blob_id).await?;
            }
            Ok(Action::Done(job))
        }
        Ok(None) => Ok(match find(&mut *conn, id).await? {
            Some(job) => Action::NotDead(job),
            None => Action::Missing,
        }),
        Err(err) if is_unique_violation(&err) => Ok(match find(&mut *conn, id).await? {
            Some(job) => Action::Duplicate(job),
            None => Action::Missing,
        }),
        Err(err) => Err(err),
    }
}

/// Forget the dead job a blob's row records: something is going to finish
/// the blob after all.
async fn clear_failure(exec: impl PgExecutor<'_>, blob_id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE blobs SET failed_at = NULL, last_error = NULL \
         WHERE id = $1 AND failed_at IS NOT NULL",
    )
    .bind(blob_id)
    .execute(exec)
    .await?;
    Ok(())
}

/// Remove a dead job. A live one is left alone: a pending job is the only
/// thing that will finish its blob.
pub(crate) async fn discard(conn: &mut PgConnection, id: i64) -> sqlx::Result<Action> {
    let deleted = sqlx::query_as::<_, Job>(
        "DELETE FROM jobs WHERE id = $1 AND failed_at IS NOT NULL RETURNING *",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match deleted {
        Some(job) => Action::Done(job),
        None => match find(&mut *conn, id).await? {
            Some(job) => Action::NotDead(job),
            None => Action::Missing,
        },
    })
}

/// The `traceparent` a job enqueued now records: the running unit's.
pub(crate) fn current_traceparent() -> Option<String> {
    TraceContext::current().map(|context| context.to_string())
}

/// Insert a job. At most one pending job of each kind exists per blob, the
/// blob-less ones counting as one blob, and duplicates are dropped. Returns
/// whether a row was inserted. Workers are notified when the surrounding
/// transaction commits. The row records the current trace context, see
/// `crate::trace`; a duplicate dropped keeps the first one's. A job that
/// finishes its blob clears the failure a dead predecessor left on the
/// blob's row, see `fail`.
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub async fn enqueue(
    exec: impl PgExecutor<'_>,
    kind: JobKind,
    blob_id: Option<Uuid>,
    priority: Priority,
    payload: serde_json::Value,
    run_at: Timestamp,
) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "WITH inserted AS (\
             INSERT INTO jobs (kind, blob_id, priority, payload, run_at, traceparent) \
             VALUES ($1, $2, $3, $4, $5, $8) \
             ON CONFLICT DO NOTHING RETURNING kind, blob_id\
         ), cleared AS (\
             UPDATE blobs SET failed_at = NULL, last_error = NULL \
             WHERE $7 AND failed_at IS NOT NULL \
               AND id IN (SELECT blob_id FROM inserted)\
         ) \
         SELECT pg_notify($6, kind) FROM inserted",
    )
    .bind(kind.as_str())
    .bind(blob_id)
    .bind(priority.rank())
    .bind(payload)
    .bind(run_at.to_sqlx())
    .bind(CHANNEL)
    .bind(kind.finishes_blob())
    .bind(current_traceparent())
    .execute(exec)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Lock the highest-priority, then oldest, runnable job of one of `kinds`
/// for `worker`. A job is runnable when it is due, not dead, and either
/// unlocked or held by a lease older than `lease`.
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub async fn claim(
    exec: impl PgExecutor<'_>,
    kinds: &[JobKind],
    worker: &str,
    lease: Duration,
) -> sqlx::Result<Option<Job>> {
    let kinds: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
    sqlx::query_as::<_, Job>(
        "UPDATE jobs SET locked_at = now(), locked_by = $1, attempts = attempts + 1 \
         WHERE id = (\
             SELECT id FROM jobs \
             WHERE kind = ANY($2) AND run_at <= now() AND failed_at IS NULL \
               AND (locked_at IS NULL OR locked_at < now() - make_interval(secs => $3)) \
             ORDER BY priority DESC, run_at, id \
             FOR UPDATE SKIP LOCKED \
             LIMIT 1\
         ) \
         RETURNING *",
    )
    .bind(worker)
    .bind(kinds)
    .bind(lease.as_secs_f64())
    .fetch_optional(exec)
    .await
}

/// Extend `worker`'s lease on a job it is running. Returns whether the job
/// was still held by `worker`: a lapsed lease may have let another worker
/// claim it, or the job may be gone.
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub async fn touch(exec: impl PgExecutor<'_>, id: i64, worker: &str) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "UPDATE jobs SET locked_at = now() \
         WHERE id = $1 AND locked_by = $2 AND failed_at IS NULL",
    )
    .bind(id)
    .bind(worker)
    .execute(exec)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Remove the dead jobs that failed before `before`: they have been around
/// long enough to have been looked at.
pub(crate) async fn prune_failed(
    exec: impl PgExecutor<'_>,
    before: Timestamp,
) -> sqlx::Result<u64> {
    let result = sqlx::query("DELETE FROM jobs WHERE failed_at IS NOT NULL AND failed_at <= $1")
        .bind(before.to_sqlx())
        .execute(exec)
        .await?;
    Ok(result.rows_affected())
}

/// A finished job leaves the table. One that finishes its blob clears the
/// verdict a dead twin may have left on the row meanwhile: the twin was
/// enqueued while this one ran, and the blob is not stuck after all.
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub async fn succeed(conn: &mut PgConnection, job: &Job) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(job.id)
        .execute(&mut *conn)
        .await?;
    if let Some(blob_id) = job.blob_id.filter(|_| job.kind.finishes_blob()) {
        clear_failure(&mut *conn, blob_id).await?;
    }
    Ok(())
}

/// What became of a failed job.
#[derive(Debug, Clone, PartialEq, Eq)]
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub enum Failure {
    /// Unlocked again, due at the given time.
    Retry(Timestamp),
    /// Out of attempts: kept, locked, marked `failed_at`.
    Dead,
    /// An identical job was enqueued meanwhile and will run instead.
    Superseded,
}

/// Record a failed attempt: retry with exponential backoff while attempts
/// remain, otherwise mark the job dead. A `permanent` failure is one a
/// retry cannot mend, see `JobError::is_permanent`; the job is dead at once.
/// A `retry_after` the failure asks for, a remote's `Retry-After`, is
/// honoured when it is longer than the backoff. A death stamped on the
/// blob is reported to the webhook, when `webhook` wants `failed`; the
/// caller runs this in a transaction so the stamp and its notification
/// commit together.
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub async fn fail(
    conn: &mut PgConnection,
    job: &Job,
    error: &str,
    permanent: bool,
    retry_after: Option<Duration>,
    webhook: &WebhookConfig,
    now: Timestamp,
) -> sqlx::Result<Failure> {
    let error = truncate(error);
    if permanent || job.attempts >= job.max_attempts {
        sqlx::query("UPDATE jobs SET failed_at = now(), last_error = $2 WHERE id = $1")
            .bind(job.id)
            .bind(&error)
            .execute(&mut *conn)
            .await?;
        // The blob keeps the verdict too: the dead job is pruned after
        // `jobs.keep_failed`, the stuck blob is not. Unless a twin of this
        // job, enqueued while it ran, is still going to finish the blob.
        if let Some(blob_id) = job.blob_id.filter(|_| job.kind.finishes_blob()) {
            let stamped = sqlx::query(
                "UPDATE blobs SET failed_at = now(), last_error = $2 \
                 WHERE id = $1 AND NOT EXISTS (\
                     SELECT 1 FROM jobs \
                     WHERE blob_id = $1 AND kind = $3 AND failed_at IS NULL AND id <> $4\
                 )",
            )
            .bind(blob_id)
            .bind(&error)
            .bind(job.kind.as_str())
            .bind(job.id)
            .execute(&mut *conn)
            .await?
            .rows_affected()
                == 1;
            if stamped && webhook.wants(notify::Event::Failed) {
                notify::enqueue(
                    &mut *conn,
                    blob_id,
                    job.priority,
                    notify::Event::Failed,
                    now,
                )
                .await?;
            }
        }
        return Ok(Failure::Dead);
    }
    let wait = retry_after
        .and_then(|d| SignedDuration::try_from(d).ok())
        .map_or(backoff(job.attempts), |asked| {
            asked.max(backoff(job.attempts))
        });
    let run_at = now + wait;
    let result = sqlx::query(
        "UPDATE jobs SET locked_at = NULL, locked_by = NULL, run_at = $2, last_error = $3 \
         WHERE id = $1",
    )
    .bind(job.id)
    .bind(run_at.to_sqlx())
    .bind(&error)
    .execute(&mut *conn)
    .await;
    match result {
        Ok(_) => Ok(Failure::Retry(run_at)),
        // Unlocking would make a second pending job of this kind for the
        // blob; the newer one carries on and this one is redundant.
        Err(err) if is_unique_violation(&err) => {
            succeed(&mut *conn, job).await?;
            Ok(Failure::Superseded)
        }
        Err(err) => Err(err),
    }
}

fn backoff(attempts: i32) -> SignedDuration {
    let factor = 2u32.saturating_pow(attempts.saturating_sub(1).clamp(0, 30) as u32);
    let delay = BACKOFF_BASE.saturating_mul(factor).min(BACKOFF_MAX);
    SignedDuration::try_from(delay).unwrap_or(SignedDuration::from_secs(3600))
}

fn is_unique_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|e| e.code())
        .is_some_and(|code| code == "23505")
}

/// `last_error` keeps the head of the message, not a whole backtrace.
fn truncate(error: &str) -> String {
    const MAX: usize = 2000;
    let mut end = error.len().min(MAX);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(1), SignedDuration::from_secs(30));
        assert_eq!(backoff(2), SignedDuration::from_secs(60));
        assert_eq!(backoff(4), SignedDuration::from_secs(240));
        assert_eq!(backoff(40), SignedDuration::from_secs(3600));
        assert_eq!(backoff(0), SignedDuration::from_secs(30));
    }

    #[test]
    fn truncates_on_char_boundaries() {
        let s = "é".repeat(2000);
        let t = truncate(&s);
        assert!(t.len() <= 2000);
        assert!(t.chars().all(|c| c == 'é'));
    }
}
