//! Public interface handlers. Every handler takes `Signed` first.

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::routing::get;
use uuid::Uuid;

use super::body;
use super::extract::{Json, Path, Query};
use super::json::PublicBlobResponse;
use super::original::{self, OriginalQuery};
use super::signed::Signed;
use crate::app::AppState;
use crate::blob::Blob;
use crate::blob::repo;
use crate::error::{ApiError, ApiResult};
use crate::upload;

/// `body_limit` caps the request body of the upload route. It is a backstop:
/// the pipeline stops at the declared size, which is at most
/// `upload.max_size`; the limit only matters for multipart framing around it.
pub fn routes(body_limit: usize) -> Router<AppState> {
    Router::new()
        .route(
            "/blobs/{token}/{id}",
            get(get_blob)
                .put(upload_blob)
                .layer(DefaultBodyLimit::max(body_limit)),
        )
        .route("/blobs/{token}/{id}/original/{filename}", get(get_original))
        .route(
            "/blobs/{token}/{id}/preview/{filename}",
            get(not_implemented),
        )
        .route(
            "/blobs/{token}/{id}/thumbnail/{filename}",
            get(not_implemented),
        )
}

async fn get_blob(
    _signed: Signed,
    State(state): State<AppState>,
    Path((_token, id)): Path<(String, Uuid)>,
) -> ApiResult<Json<PublicBlobResponse>> {
    let blob = find_visible(&state, id).await?;
    Ok(Json(PublicBlobResponse::new(&blob)))
}

async fn upload_blob(
    _signed: Signed,
    State(state): State<AppState>,
    Path((_token, id)): Path<(String, Uuid)>,
    req: Request,
) -> ApiResult<Json<PublicBlobResponse>> {
    let blob = find_visible(&state, id).await?;
    // Defense in depth: minting already refuses these, but a leaked key must
    // not turn into a policy bypass.
    if !blob.public_upload_allowed() {
        return Err(ApiError::Forbidden(
            "this blob can only be uploaded through the private interface".into(),
        ));
    }
    if !blob.accepts_upload() {
        return Err(ApiError::Conflict(format!(
            "blob is {} and cannot be uploaded",
            blob.state
        )));
    }
    // The public port never overrides what the backend declared.
    let declared = upload::resolve(&blob, None)?;
    let blob = body::upload(&state, blob, declared, req).await?;
    Ok(Json(PublicBlobResponse::new(&blob)))
}

async fn get_original(
    _signed: Signed,
    State(state): State<AppState>,
    Path((_token, id, filename)): Path<(String, Uuid, String)>,
    Query(query): Query<OriginalQuery>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let blob = find_visible(&state, id).await?;
    original::serve(&state, blob, &filename, query, &method, &headers).await
}

/// Look a blob up as the public port sees it: terminal states are `404`.
async fn find_visible(state: &AppState, id: Uuid) -> ApiResult<Blob> {
    repo::find(state.pool(), id)
        .await?
        .filter(Blob::visible_on_public)
        .ok_or(ApiError::NotFound)
}

async fn not_implemented(_signed: Signed) -> ApiError {
    ApiError::NotImplemented
}
