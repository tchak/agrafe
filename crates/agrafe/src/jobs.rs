//! Postgres-backed job queue: the `jobs` table, claimed with
//! `FOR UPDATE SKIP LOCKED` and woken by `NOTIFY`. The workers live in
//! `runner`, the handlers in their own modules.

pub mod purge;
pub mod reap;
pub mod render;
pub mod runner;
pub mod scan;

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use jiff_sqlx::ToSqlx;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;

/// Why one attempt at a job failed. The message is what `last_error` keeps.
#[derive(Debug, thiserror::Error)]
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
    Render(#[from] crate::media::render::Error),
    #[error(transparent)]
    Sandbox(#[from] crate::media::sandbox::Error),
    #[error(transparent)]
    Representation(#[from] crate::blob::representation::Error),
    #[error("purge: {0}")]
    Purge(#[from] purge::PurgeError),
}

/// Channel every enqueue notifies; the payload is the job kind.
pub const CHANNEL: &str = "agrafe_jobs";

/// Delay before the first retry; doubles with every failure.
const BACKOFF_BASE: Duration = Duration::from_secs(30);
const BACKOFF_MAX: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Scan,
    /// Thumbnail and preview, whichever is pending.
    Render,
    Purge,
    Reap,
}

crate::str_enum!(JobKind {
    Scan => "scan",
    Render => "render",
    Purge => "purge",
    Reap => "reap",
});

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Job {
    pub id: i64,
    #[sqlx(try_from = "String")]
    pub kind: JobKind,
    pub blob_id: Option<Uuid>,
    pub payload: serde_json::Value,
    #[sqlx(try_from = "jiff_sqlx::Timestamp")]
    pub run_at: Timestamp,
    pub attempts: i32,
    pub max_attempts: i32,
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub locked_at: Option<Timestamp>,
    pub locked_by: Option<String>,
    pub last_error: Option<String>,
    #[sqlx(try_from = "crate::db::NullableTimestamp")]
    pub failed_at: Option<Timestamp>,
}

/// Insert a job. At most one pending job of each kind exists per blob, the
/// blob-less ones counting as one blob, and duplicates are dropped. Returns
/// whether a row was inserted. Workers are notified when the surrounding
/// transaction commits.
pub async fn enqueue(
    exec: impl PgExecutor<'_>,
    kind: JobKind,
    blob_id: Option<Uuid>,
    payload: serde_json::Value,
    run_at: Timestamp,
) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "WITH inserted AS (\
             INSERT INTO jobs (kind, blob_id, payload, run_at) VALUES ($1, $2, $3, $4) \
             ON CONFLICT DO NOTHING RETURNING kind\
         ) \
         SELECT pg_notify($5, kind) FROM inserted",
    )
    .bind(kind.as_str())
    .bind(blob_id)
    .bind(payload)
    .bind(run_at.to_sqlx())
    .bind(CHANNEL)
    .execute(exec)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Lock the oldest runnable job of one of `kinds` for `worker`. A job is
/// runnable when it is due, not dead, and either unlocked or held by a lease
/// older than `lease`.
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
             ORDER BY run_at, id \
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
pub async fn prune_failed(exec: impl PgExecutor<'_>, before: Timestamp) -> sqlx::Result<u64> {
    let result = sqlx::query("DELETE FROM jobs WHERE failed_at IS NOT NULL AND failed_at <= $1")
        .bind(before.to_sqlx())
        .execute(exec)
        .await?;
    Ok(result.rows_affected())
}

/// A finished job leaves the table.
pub async fn succeed(exec: impl PgExecutor<'_>, id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(id)
        .execute(exec)
        .await?;
    Ok(())
}

/// What became of a failed job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// Unlocked again, due at the given time.
    Retry(Timestamp),
    /// Out of attempts: kept, locked, marked `failed_at`.
    Dead,
    /// An identical job was enqueued meanwhile and will run instead.
    Superseded,
}

/// Record a failed attempt: retry with exponential backoff while attempts
/// remain, otherwise mark the job dead.
pub async fn fail(
    conn: &mut PgConnection,
    job: &Job,
    error: &str,
    now: Timestamp,
) -> sqlx::Result<Failure> {
    let error = truncate(error);
    if job.attempts >= job.max_attempts {
        sqlx::query("UPDATE jobs SET failed_at = now(), last_error = $2 WHERE id = $1")
            .bind(job.id)
            .bind(&error)
            .execute(&mut *conn)
            .await?;
        return Ok(Failure::Dead);
    }
    let run_at = now + backoff(job.attempts);
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
            succeed(&mut *conn, job.id).await?;
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
