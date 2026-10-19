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
use super::original::{self, Original, OriginalQuery};
use super::representation::{self, Port, RepresentationQuery};
use super::signed::Signed;
use agrafe::Agrafe;
use agrafe::blob::envelope::Envelope;
use agrafe::blob::{BlobHead, Representation};

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
    // The upload needs the whole row; the terminal states are hidden as
    // for every other public route.
    let blob = state
        .find_blob(id)
        .await?
        .filter(|blob| blob.visible_on_public(Timestamp::now()))
        .ok_or(ApiError::NotFound)?;
    // Defense in depth: minting already leaves these out, but a leaked key must
    // not turn into a policy bypass.
    if !blob.public_upload_allowed() {
        return Err(ApiError::Forbidden(
            "this blob can only be uploaded through the private interface".into(),
        ));
    }
    let blob = body::upload(&state, blob, req)
        .await
        .map_err(|err| match err {
            // Which signature caught the file is an evasion hint for an
            // anonymous uploader; the backend reads it from the metadata.
            ApiError::Infected(_) => ApiError::Infected(None),
            other => other,
        })?;
    Ok(Json(PublicBlobResponse::new(&blob.head())))
}

async fn get_original(
    _signed: Signed,
    State(state): State<Agrafe>,
    Path((_token, id, filename)): Path<(String, Uuid, String)>,
    Query(query): Query<OriginalQuery>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    // Sealed: the envelope is the row, and nothing is looked up.
    let blob = match query.e.as_deref().map(decode_envelope).transpose()? {
        Some(envelope) => Original::from_envelope(id, filename, envelope)?,
        None => Original::from_row(find_visible(&state, id).await?, &filename)?,
    };
    original::serve(&state, blob, query, &method, &headers).await
}

/// An envelope that does not decode is a minter's mistake: the URL was
/// signed, so it is no forgery.
fn decode_envelope(e: &str) -> ApiResult<Envelope> {
    Envelope::decode(e).map_err(|err| ApiError::BadRequest(err.to_string()))
}

/// Look a blob up as the public port sees it: terminal states are `404`.
async fn find_visible(state: &Agrafe, id: Uuid) -> ApiResult<BlobHead> {
    state
        .find_blob_head(id)
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
    serve_representation(
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

/// Sealed when the URL carries an envelope that has `kind` rendered; from
/// the row otherwise, as when a render was still pending at minting.
async fn serve_representation(
    state: Agrafe,
    id: Uuid,
    kind: Representation,
    filename: String,
    query: RepresentationQuery,
    method: Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    if let Some(e) = &query.e {
        let envelope = decode_envelope(e)?;
        if let Some(response) = representation::serve_sealed(
            &state, id, &envelope, kind, &filename, &query, &method, &headers,
        )
        .await?
        {
            return Ok(response);
        }
    }
    let blob = find_visible(&state, id).await?;
    representation::serve(
        &state,
        blob,
        kind,
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
    serve_representation(
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
