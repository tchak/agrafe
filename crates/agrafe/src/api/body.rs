//! Turning a `PUT` request into the stream of file bytes the upload pipeline
//! consumes, according to the blob's input type.

use axum::extract::multipart::Multipart;
use axum::extract::{FromRequest, Request};
use futures::TryStreamExt;

use crate::app::AppState;
use crate::blob::{Blob, InputType};
use crate::error::{ApiError, ApiResult};
use crate::upload::{self, Declared};

/// The multipart part holding the file.
pub const FILE_PART: &str = "file";

pub async fn upload(
    state: &AppState,
    blob: Blob,
    declared: Declared,
    req: Request,
) -> ApiResult<Blob> {
    match blob.input_type {
        InputType::Raw => {
            let body = req
                .into_body()
                .into_data_stream()
                .map_err(|e| ApiError::BadRequest(format!("reading the request body: {e}")));
            upload::run(state, blob, declared, body).await
        }
        InputType::Multipart => {
            let mut multipart = Multipart::from_request(req, &())
                .await
                .map_err(|e| ApiError::from_rejection(e.status(), e.body_text()))?;
            let field = loop {
                match multipart.next_field().await.map_err(multipart_error)? {
                    Some(field) if field.name() == Some(FILE_PART) => break field,
                    Some(_) => continue,
                    None => {
                        return Err(ApiError::Unprocessable(format!(
                            "multipart body has no part named `{FILE_PART}`"
                        )));
                    }
                }
            };
            let body = field.map_err(multipart_error);
            upload::run(state, blob, declared, body).await
        }
        InputType::Rows | InputType::Archive => Err(ApiError::NotImplemented),
    }
}

fn multipart_error(e: axum::extract::multipart::MultipartError) -> ApiError {
    ApiError::from_rejection(e.status(), e.body_text())
}
