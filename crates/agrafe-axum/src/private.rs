//! Private interface handlers.

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::{get, post, put};
use serde::Deserialize;
use uuid::Uuid;

use agrafe::Agrafe;
use agrafe::attachment::Attachment;
use agrafe::blob::Blob;
use agrafe::blob::create::CreateBlobRequest;
use agrafe::blob::urls::{BlobUrls, MintOptions};
use agrafe::blob::{self, ListParams};
use agrafe::blob::{BlobState, Representation, ScanState};
use agrafe::classify::Category;

use super::body;
use super::extract::{Json, Path, Query};
use super::json::{BlobResponse, ListResponse};
use super::original::{self, OriginalQuery};
use super::representation::{self, Port, RepresentationQuery};
use crate::error::{ApiError, ApiResult};

/// Request body limit of `POST /blobs`. The widest sheet, 16 384 columns
/// with a kilobyte of name each, fits; axum's default of 2 MiB would not
/// hold it.
const CREATE_BODY_LIMIT: usize = 16 * 1024 * 1024;

/// `body_limit` caps the request body of the upload route. It is a backstop:
/// the pipeline stops at the declared size, which is at most
/// `upload.max_size`, and the multipart framing around it at
/// `MULTIPART_OVERHEAD` (see `body::framing`).
pub fn routes(body_limit: usize) -> Router<Agrafe> {
    Router::new()
        .route(
            "/blobs",
            post(create_blob)
                .get(list_blobs)
                .layer(DefaultBodyLimit::max(CREATE_BODY_LIMIT)),
        )
        .route(
            "/blobs/{id}",
            get(get_blob)
                .put(upload_blob)
                .delete(delete_blob)
                .layer(DefaultBodyLimit::max(body_limit)),
        )
        .route(
            "/blobs/{id}/parts/{n}",
            put(upload_part).layer(DefaultBodyLimit::max(body_limit)),
        )
        .route("/blobs/{id}/finish", post(finish_source))
        .route("/blobs/{id}/retry", post(retry_blob))
        .route("/blobs/{id}/urls", post(mint_urls))
        .route("/blobs/{id}/original/{filename}", get(get_original))
        .route("/blobs/{id}/preview/{filename}", get(get_preview))
        .route("/blobs/{id}/thumbnail/{filename}", get(get_thumbnail))
}

/// The full metadata of `blob`: its attachments and ready-made URLs.
async fn respond(agrafe: &Agrafe, blob: &Blob) -> ApiResult<BlobResponse> {
    Ok(agrafe.blob_metadata(blob).await?)
}

/// `respond` for a page of blobs, with one query for their attachments.
async fn respond_many(agrafe: &Agrafe, blobs: &[Blob]) -> ApiResult<Vec<BlobResponse>> {
    let ids: Vec<Uuid> = blobs.iter().map(|b| b.id).collect();
    let attachments = agrafe.attachments_for(&ids).await?;
    Ok(blobs
        .iter()
        .map(|blob| {
            let own: Vec<Attachment> = attachments
                .iter()
                .filter(|a| a.blob_id == blob.id)
                .cloned()
                .collect();
            BlobResponse::new(blob, &own, agrafe.urls(blob))
        })
        .collect())
}

async fn create_blob(
    State(agrafe): State<Agrafe>,
    Json(req): Json<CreateBlobRequest>,
) -> ApiResult<impl IntoResponse> {
    let blob = agrafe.create_blob(req).await?;
    Ok((StatusCode::CREATED, Json(respond(&agrafe, &blob).await?)))
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub content_type: Option<String>,
    pub category: Option<Category>,
    pub state: Option<BlobState>,
    pub scan_state: Option<ScanState>,
    pub attached: Option<bool>,
    pub failed: Option<bool>,
    pub cursor: Option<Uuid>,
    pub limit: Option<usize>,
}

async fn list_blobs(
    State(agrafe): State<Agrafe>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<ListResponse>> {
    let params = ListParams {
        content_type: q.content_type,
        category: q.category,
        state: q.state,
        scan_state: q.scan_state,
        attached: q.attached,
        failed: q.failed,
        cursor: q.cursor,
        limit: q.limit.unwrap_or(blob::DEFAULT_LIMIT),
    };
    let page = agrafe.list_blobs(params).await?;
    let items = respond_many(&agrafe, &page.items).await?;
    Ok(Json(ListResponse {
        items,
        next_cursor: page.next_cursor,
    }))
}

async fn get_blob(
    State(agrafe): State<Agrafe>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<BlobResponse>> {
    let blob = agrafe.find_blob(id).await?.ok_or(ApiError::NotFound)?;
    Ok(Json(respond(&agrafe, &blob).await?))
}

async fn upload_blob(
    State(agrafe): State<Agrafe>,
    Path(id): Path<Uuid>,
    req: Request,
) -> ApiResult<Json<BlobResponse>> {
    let blob = agrafe.find_blob(id).await?.ok_or(ApiError::NotFound)?;
    let blob = body::upload(&agrafe, blob, req).await?;
    Ok(Json(respond(&agrafe, &blob).await?))
}

async fn upload_part(
    State(agrafe): State<Agrafe>,
    Path((id, n)): Path<(Uuid, u32)>,
    req: Request,
) -> ApiResult<Json<BlobResponse>> {
    let blob = agrafe.find_blob(id).await?.ok_or(ApiError::NotFound)?;
    let blob = body::upload_part(&agrafe, blob, n, req).await?;
    Ok(Json(respond(&agrafe, &blob).await?))
}

async fn finish_source(
    State(agrafe): State<Agrafe>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<BlobResponse>> {
    let blob = agrafe.finish_source(id).await?;
    Ok(Json(respond(&agrafe, &blob).await?))
}

async fn delete_blob(State(agrafe): State<Agrafe>, Path(id): Path<Uuid>) -> ApiResult<StatusCode> {
    if agrafe.delete_blob(id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

/// Enqueue whatever the blob is waiting on: the repair for a blob whose
/// fetch, scan, build or render died, from the blob's side. `202`: the job is
/// queued, the blob is not finished yet.
async fn retry_blob(
    State(agrafe): State<Agrafe>,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let blob = agrafe.retry_blob(id).await?;
    Ok((StatusCode::ACCEPTED, Json(respond(&agrafe, &blob).await?)))
}

async fn mint_urls(
    State(agrafe): State<Agrafe>,
    Path(id): Path<Uuid>,
    body: Option<Json<MintOptions>>,
) -> ApiResult<Json<BlobUrls>> {
    let Json(opts) = body.unwrap_or_default();
    Ok(Json(agrafe.mint_urls(id, &opts).await?))
}

async fn get_original(
    State(agrafe): State<Agrafe>,
    Path((id, filename)): Path<(Uuid, String)>,
    Query(query): Query<OriginalQuery>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let blob = agrafe.find_blob_head(id).await?.ok_or(ApiError::NotFound)?;
    let blob = original::Original::from_row(blob, &filename)?;
    original::serve(&agrafe, blob, query, &method, &headers).await
}

async fn get_preview(
    State(state): State<Agrafe>,
    Path((id, filename)): Path<(Uuid, String)>,
    Query(query): Query<RepresentationQuery>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    get_representation(
        state,
        id,
        Representation::Preview,
        filename,
        query,
        method,
        headers,
    )
    .await
}

async fn get_thumbnail(
    State(state): State<Agrafe>,
    Path((id, filename)): Path<(Uuid, String)>,
    Query(query): Query<RepresentationQuery>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    get_representation(
        state,
        id,
        Representation::Thumbnail,
        filename,
        query,
        method,
        headers,
    )
    .await
}

async fn get_representation(
    state: Agrafe,
    id: Uuid,
    kind: Representation,
    filename: String,
    query: RepresentationQuery,
    method: Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let blob = state.find_blob_head(id).await?.ok_or(ApiError::NotFound)?;
    representation::serve(
        &state,
        blob,
        kind,
        &filename,
        query,
        &method,
        &headers,
        Port::Private,
    )
    .await
}
