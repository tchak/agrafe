//! Public interface handlers. Every handler takes `Signed` first.

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::routing::get;
use jiff::Timestamp;
use uuid::Uuid;

use super::body;
use super::extract::{Json, Path, Query};
use super::json::PublicBlobResponse;
use super::original::{self, OriginalQuery};
use super::representation::{self, Port, RepresentationQuery};
use super::signed::Signed;
use agrafe::Agrafe;
use agrafe::blob::{Blob, Representation};
use agrafe::upload;

use crate::error::{ApiError, ApiResult};

/// `body_limit` caps the request body of the upload route. It is a backstop:
/// the pipeline stops at the declared size, which is at most
/// `upload.max_size`; the limit only matters for multipart framing around it.
pub fn routes(body_limit: usize) -> Router<Agrafe> {
    Router::new()
        .route(
            "/blobs/{token}/{id}",
            get(get_blob)
                .put(upload_blob)
                .layer(DefaultBodyLimit::max(body_limit)),
        )
        .route("/blobs/{token}/{id}/original/{filename}", get(get_original))
        .route("/blobs/{token}/{id}/preview/{filename}", get(get_preview))
        .route(
            "/blobs/{token}/{id}/thumbnail/{filename}",
            get(get_thumbnail),
        )
}

async fn get_blob(
    _signed: Signed,
    State(state): State<Agrafe>,
    Path((_token, id)): Path<(String, Uuid)>,
) -> ApiResult<Json<PublicBlobResponse>> {
    let blob = find_visible(&state, id).await?;
    Ok(Json(PublicBlobResponse::new(&blob)))
}

async fn upload_blob(
    _signed: Signed,
    State(state): State<Agrafe>,
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
    let declared = upload::prepare(&blob, Timestamp::now())?;
    let blob = body::upload(&state, blob, declared, req)
        .await
        .map_err(|err| match err {
            // Which signature caught the file is an evasion hint for an
            // anonymous uploader; the backend reads it from the metadata.
            ApiError::Infected(_) => ApiError::Infected(None),
            other => other,
        })?;
    Ok(Json(PublicBlobResponse::new(&blob)))
}

async fn get_original(
    _signed: Signed,
    State(state): State<Agrafe>,
    Path((_token, id, filename)): Path<(String, Uuid, String)>,
    Query(query): Query<OriginalQuery>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let blob = find_visible(&state, id).await?;
    original::serve(&state, blob, &filename, query, &method, &headers).await
}

/// Look a blob up as the public port sees it: terminal states are `404`.
async fn find_visible(state: &Agrafe, id: Uuid) -> ApiResult<Blob> {
    state
        .find_blob(id)
        .await?
        .filter(|blob| blob.visible_on_public(Timestamp::now()))
        .ok_or(ApiError::NotFound)
}

async fn get_preview(
    _signed: Signed,
    State(state): State<Agrafe>,
    Path((_token, id, filename)): Path<(String, Uuid, String)>,
    Query(query): Query<RepresentationQuery>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let blob = find_visible(&state, id).await?;
    representation::serve(
        &state,
        blob,
        Representation::Preview,
        &filename,
        query,
        &method,
        &headers,
        Port::Public,
    )
    .await
}

async fn get_thumbnail(
    _signed: Signed,
    State(state): State<Agrafe>,
    Path((_token, id, filename)): Path<(String, Uuid, String)>,
    Query(query): Query<RepresentationQuery>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let blob = find_visible(&state, id).await?;
    representation::serve(
        &state,
        blob,
        Representation::Thumbnail,
        &filename,
        query,
        &method,
        &headers,
        Port::Public,
    )
    .await
}
