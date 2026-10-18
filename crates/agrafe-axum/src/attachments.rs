//! Private interface: attachments, the backend's statements about which of
//! its records use a blob. See DESIGN.md, "Attachments". The public port
//! never sees them.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use agrafe::Agrafe;
use agrafe::attachment::{self, Attached, AttachmentRequest, ListParams};
use agrafe::blob::BlobState;

use super::extract::{Json, Path, Query};
use super::json::AttachmentResponse;
use crate::error::{ApiError, ApiResult};

pub fn routes() -> Router<Agrafe> {
    Router::new()
        .route("/blobs/{id}/attachments", post(attach))
        .route("/attachments", get(list_attachments))
        .route("/attachments/{id}", get(get_attachment).delete(detach))
}

/// `201` for a new attachment, `200` for the one the triple already had.
async fn attach(
    State(agrafe): State<Agrafe>,
    Path(id): Path<Uuid>,
    Json(req): Json<AttachmentRequest>,
) -> ApiResult<impl IntoResponse> {
    let (status, attachment) = match agrafe.attach_blob(id, req).await? {
        Attached::Created(a) => (StatusCode::CREATED, a),
        Attached::Existing(a) => (StatusCode::OK, a),
    };
    Ok((status, Json(AttachmentResponse::new(&attachment))))
}

/// An attachment as the listing shows it: with the state of its blob.
#[derive(Debug, Serialize)]
pub struct AttachmentListItem {
    #[serde(flatten)]
    pub attachment: AttachmentResponse,
    pub blob_state: BlobState,
}

#[derive(Debug, Serialize)]
pub struct AttachmentListResponse {
    pub items: Vec<AttachmentListItem>,
    pub next_cursor: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct AttachmentListQuery {
    pub record_type: Option<String>,
    pub record_id: Option<String>,
    pub name: Option<String>,
    pub blob_id: Option<Uuid>,
    pub cursor: Option<Uuid>,
    pub limit: Option<usize>,
}

async fn list_attachments(
    State(agrafe): State<Agrafe>,
    Query(q): Query<AttachmentListQuery>,
) -> ApiResult<Json<AttachmentListResponse>> {
    let params = ListParams {
        record_type: q.record_type,
        record_id: q.record_id,
        name: q.name,
        blob_id: q.blob_id,
        cursor: q.cursor,
        limit: q.limit.unwrap_or(attachment::DEFAULT_LIMIT),
    };
    let page = agrafe.list_attachments(&params).await?;
    Ok(Json(AttachmentListResponse {
        items: page
            .items
            .iter()
            .map(|listed| AttachmentListItem {
                attachment: AttachmentResponse::new(&listed.attachment),
                blob_state: listed.blob_state,
            })
            .collect(),
        next_cursor: page.next_cursor,
    }))
}

async fn get_attachment(
    State(agrafe): State<Agrafe>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<AttachmentResponse>> {
    let attachment = agrafe
        .find_attachment(id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(AttachmentResponse::new(&attachment)))
}

async fn detach(State(agrafe): State<Agrafe>, Path(id): Path<Uuid>) -> ApiResult<StatusCode> {
    if agrafe.detach_blob(id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}
