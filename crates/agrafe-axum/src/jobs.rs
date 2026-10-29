//! Private interface: the job queue, for operators. Jobs are listed in any
//! state; only a dead one can be retried or discarded, since a pending job
//! is the only thing that will finish its blob.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use agrafe::Agrafe;
use agrafe::jobs::{self, Action, Cursor, Job, JobKind, JobState, ListParams, Priority};

use super::extract::{Json, Path, Query};
use crate::error::{ApiError, ApiResult};

pub fn routes() -> Router<Agrafe> {
    Router::new()
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job).delete(discard_job))
        .route("/jobs/{id}/retry", post(retry_job))
}

#[derive(Debug, Serialize)]
pub struct JobResponse {
    pub id: i64,
    pub kind: JobKind,
    pub state: JobState,
    pub blob_id: Option<Uuid>,
    pub priority: Priority,
    pub payload: serde_json::Value,
    pub run_at: Timestamp,
    pub attempts: i32,
    pub max_attempts: i32,
    pub locked_at: Option<Timestamp>,
    pub locked_by: Option<String>,
    pub last_error: Option<String>,
    pub failed_at: Option<Timestamp>,
    /// The trace of the request or job that enqueued it, which its log
    /// lines carry as `trace_id`.
    pub trace_id: Option<String>,
    pub created_at: Timestamp,
}

impl JobResponse {
    fn new(state: &Agrafe, job: Job) -> Self {
        let job_state = state.job_state(&job);
        let trace_id = job.trace_context().map(|c| c.trace_id().to_string());
        Self {
            id: job.id,
            kind: job.kind,
            state: job_state,
            blob_id: job.blob_id,
            priority: job.priority,
            payload: job.payload,
            run_at: job.run_at,
            attempts: job.attempts,
            max_attempts: job.max_attempts,
            locked_at: job.locked_at,
            locked_by: job.locked_by,
            last_error: job.last_error,
            failed_at: job.failed_at,
            trace_id,
            created_at: job.created_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JobListResponse {
    pub items: Vec<JobResponse>,
    pub next_cursor: Option<Cursor>,
}

#[derive(Debug, Deserialize)]
pub struct JobListQuery {
    pub kind: Option<JobKind>,
    pub state: Option<JobState>,
    pub priority: Option<Priority>,
    pub blob_id: Option<Uuid>,
    pub cursor: Option<Cursor>,
    pub limit: Option<usize>,
}

async fn list_jobs(
    State(state): State<Agrafe>,
    Query(q): Query<JobListQuery>,
) -> ApiResult<Json<JobListResponse>> {
    let params = ListParams {
        kind: q.kind,
        state: q.state,
        priority: q.priority,
        blob_id: q.blob_id,
        cursor: q.cursor,
        limit: q.limit.unwrap_or(jobs::DEFAULT_LIMIT),
    };
    let page = state.list_jobs(&params).await?;
    Ok(Json(JobListResponse {
        items: page
            .items
            .into_iter()
            .map(|j| JobResponse::new(&state, j))
            .collect(),
        next_cursor: page.next_cursor,
    }))
}

async fn get_job(State(state): State<Agrafe>, Path(id): Path<i64>) -> ApiResult<Json<JobResponse>> {
    let job = state.find_job(id).await?.ok_or(ApiError::NotFound)?;
    Ok(Json(JobResponse::new(&state, job)))
}

async fn retry_job(
    State(state): State<Agrafe>,
    Path(id): Path<i64>,
) -> ApiResult<Json<JobResponse>> {
    match state.retry_job(id).await? {
        Action::Done(job) => Ok(Json(JobResponse::new(&state, job))),
        Action::NotDead(job) => Err(not_dead(&state, &job)),
        Action::Duplicate(job) => Err(ApiError::Conflict(format!(
            "a {} job is already pending for this blob",
            job.kind
        ))),
        Action::Missing => Err(ApiError::NotFound),
    }
}

async fn discard_job(State(state): State<Agrafe>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    match state.discard_job(id).await? {
        Action::Done(_) => Ok(StatusCode::NO_CONTENT),
        Action::NotDead(job) | Action::Duplicate(job) => Err(not_dead(&state, &job)),
        Action::Missing => Err(ApiError::NotFound),
    }
}

fn not_dead(state: &Agrafe, job: &Job) -> ApiError {
    ApiError::Conflict(format!(
        "job is {}, only a dead job can be retried or discarded",
        state.job_state(job)
    ))
}
