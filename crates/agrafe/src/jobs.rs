//! Postgres-backed job queue. Enqueue only, for now; the runner comes with
//! the scanning and media work.

use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use serde::{Deserialize, Serialize};
use sqlx::PgExecutor;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Scan,
    Thumbnail,
    Preview,
    Purge,
    Reap,
}

crate::str_enum!(JobKind {
    Scan => "scan",
    Thumbnail => "thumbnail",
    Preview => "preview",
    Purge => "purge",
    Reap => "reap",
});

/// Insert a job. With a `blob_id`, at most one pending job of each kind exists
/// per blob and duplicates are dropped. Returns whether a row was inserted.
pub async fn enqueue(
    exec: impl PgExecutor<'_>,
    kind: JobKind,
    blob_id: Option<Uuid>,
    payload: serde_json::Value,
    run_at: Timestamp,
) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "INSERT INTO jobs (kind, blob_id, payload, run_at) VALUES ($1, $2, $3, $4) \
         ON CONFLICT DO NOTHING",
    )
    .bind(kind.as_str())
    .bind(blob_id)
    .bind(payload)
    .bind(run_at.to_sqlx())
    .execute(exec)
    .await?;
    Ok(result.rows_affected() == 1)
}
