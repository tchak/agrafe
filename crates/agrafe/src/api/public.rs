//! Public interface handlers. Every handler takes `Signed` first.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use uuid::Uuid;

use super::extract::{Json, Path};
use super::json::PublicBlobResponse;
use super::signed::Signed;
use crate::app::AppState;
use crate::blob::BlobState;
use crate::blob::repo;
use crate::error::{ApiError, ApiResult};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/blobs/{token}/{id}", get(get_blob).put(upload_blob))
        .route(
            "/blobs/{token}/{id}/original/{filename}",
            get(not_implemented),
        )
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
    let blob = repo::find(state.pool(), id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if blob.state == BlobState::Deleted {
        return Err(ApiError::NotFound);
    }
    Ok(Json(PublicBlobResponse::new(&blob)))
}

async fn upload_blob(
    _signed: Signed,
    State(state): State<AppState>,
    Path((_token, id)): Path<(String, Uuid)>,
) -> ApiResult<StatusCode> {
    let blob = repo::find(state.pool(), id)
        .await?
        .ok_or(ApiError::NotFound)?;
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
    Err(ApiError::NotImplemented)
}

async fn not_implemented(_signed: Signed) -> ApiError {
    ApiError::NotImplemented
}
