//! Private interface handlers.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use super::json::{BlobResponse, ListResponse};
use super::urls::{self, MintOptions};
use crate::app::AppState;
use crate::blob::create::{self, CreateBlobRequest};
use crate::blob::repo::{self, ListParams, Transition};
use crate::blob::{BlobState, ScanState};
use crate::classify::Category;
use crate::error::{ApiError, ApiResult};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/blobs", post(create_blob).get(list_blobs))
        .route(
            "/blobs/{id}",
            get(get_blob).put(upload_blob).delete(delete_blob),
        )
        .route("/blobs/{id}/urls", post(mint_urls))
        .route("/blobs/{id}/original/{filename}", get(not_implemented))
        .route("/blobs/{id}/preview/{filename}", get(not_implemented))
        .route("/blobs/{id}/thumbnail/{filename}", get(not_implemented))
}

async fn create_blob(
    State(state): State<AppState>,
    Json(req): Json<CreateBlobRequest>,
) -> ApiResult<impl IntoResponse> {
    let new = create::validate(req, &state.config().upload, Utc::now())?;
    let blob = repo::insert(state.pool(), &new).await?;
    let urls = urls::mint(&state, &blob, &MintOptions::default(), SystemTime::now());
    Ok((StatusCode::CREATED, Json(BlobResponse::new(&blob, urls))))
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub content_type: Option<String>,
    pub category: Option<Category>,
    pub state: Option<BlobState>,
    pub scan_state: Option<ScanState>,
    pub cursor: Option<Uuid>,
    pub limit: Option<usize>,
}

async fn list_blobs(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<ListResponse>> {
    let content_type = match q.content_type {
        Some(ct) => Some(create::normalize_content_type(&ct)?),
        None => None,
    };
    let params = ListParams {
        content_type,
        category: q.category,
        state: q.state,
        scan_state: q.scan_state,
        cursor: q.cursor,
        limit: q.limit.unwrap_or(repo::DEFAULT_LIMIT),
    };
    let page = repo::list(state.pool(), &params).await?;
    let now = SystemTime::now();
    let items = page
        .items
        .iter()
        .map(|b| BlobResponse::new(b, urls::mint(&state, b, &MintOptions::default(), now)))
        .collect();
    Ok(Json(ListResponse {
        items,
        next_cursor: page.next_cursor,
    }))
}

async fn get_blob(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<BlobResponse>> {
    let blob = repo::find(state.pool(), id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let urls = urls::mint(&state, &blob, &MintOptions::default(), SystemTime::now());
    Ok(Json(BlobResponse::new(&blob, urls)))
}

async fn upload_blob(State(state): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<StatusCode> {
    let blob = repo::find(state.pool(), id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !blob.accepts_upload() {
        return Err(ApiError::Conflict(format!(
            "blob is {} and cannot be uploaded",
            blob.state
        )));
    }
    Err(ApiError::NotImplemented)
}

async fn delete_blob(State(state): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<StatusCode> {
    match repo::soft_delete(state.pool(), id).await? {
        Transition::Done(_) => Ok(StatusCode::NO_CONTENT),
        // Already deleted: idempotent.
        Transition::Conflict(_) => Ok(StatusCode::NO_CONTENT),
        Transition::Missing => Err(ApiError::NotFound),
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MintRequest {
    #[serde(default, with = "humantime_serde")]
    pub ttl: Option<Duration>,
    #[serde(default)]
    pub permanent: bool,
    #[serde(default)]
    pub query: BTreeMap<String, String>,
}

async fn mint_urls(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<MintRequest>>,
) -> ApiResult<Json<urls::BlobUrls>> {
    let Json(req) = body.unwrap_or_default();
    if req.permanent && !state.signer().allow_permanent() {
        return Err(ApiError::Unprocessable(
            "permanent URLs are disabled (signing.allow_permanent)".into(),
        ));
    }
    if req.permanent && req.ttl.is_some() {
        return Err(ApiError::Unprocessable(
            "ttl and permanent are mutually exclusive".into(),
        ));
    }
    let blob = repo::find(state.pool(), id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let opts = MintOptions {
        ttl: req.ttl,
        permanent: req.permanent,
        query: req.query,
    };
    Ok(Json(urls::mint(&state, &blob, &opts, SystemTime::now())))
}

async fn not_implemented() -> ApiError {
    ApiError::NotImplemented
}
