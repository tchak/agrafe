//! Turning a `PUT` request into the stream of file bytes the upload pipeline
//! consumes, according to the blob's input type, and handing it to the
//! `Agrafe` handle.

use axum::RequestExt;
use axum::extract::multipart::Multipart;
use axum::extract::{FromRequest, Request};
use futures::TryStreamExt;
use http_body_util::LengthLimitError;
use jiff::Timestamp;

use agrafe::Agrafe;
use agrafe::blob::{Blob, InputType};
use agrafe::upload::{self, BoxError};

use crate::error::{ApiError, ApiResult};

/// The multipart part holding the file.
pub const FILE_PART: &str = "file";

/// Upload the body of `req` as the bytes of `blob`. The handle runs the
/// same checks again; they run here first so that a blob which cannot take
/// an upload is refused before any of the body is read.
pub async fn upload(state: &Agrafe, blob: Blob, req: Request) -> ApiResult<Blob> {
    let declared = upload::prepare(&blob, Timestamp::now())?;
    match blob.input_type {
        InputType::Raw => {
            // A body whose length is already known not to match is refused
            // before a byte is read or the blob is touched.
            if let Some(len) = content_length(&req)
                && len != declared.size
            {
                return Err(ApiError::Unprocessable(format!(
                    "Content-Length {len} does not match the declared size {}",
                    declared.size
                )));
            }
            // `into_limited_body` applies the route's `DefaultBodyLimit`,
            // which a bare `Request` does not; `Multipart` does it itself.
            let body = req
                .into_limited_body()
                .into_data_stream()
                .map_err(body_error);
            Ok(state.upload(blob, body).await?)
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
            let body = field.map_err(|e| BoxError::from(multipart_error(e)));
            Ok(state.upload(blob, body).await?)
        }
        InputType::Rows | InputType::Archive => Err(ApiError::NotImplemented),
    }
}

fn content_length(req: &Request) -> Option<u64> {
    req.headers()
        .get(axum::http::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Body errors travel through the pipeline boxed and come back out as the
/// `ApiError` put in here (see `From<UploadError> for ApiError`).
fn body_error(e: axum::Error) -> BoxError {
    let inner = e.into_inner();
    if inner.downcast_ref::<LengthLimitError>().is_some() {
        return ApiError::PayloadTooLarge("request body exceeds the upload size limit".into())
            .into();
    }
    ApiError::BadRequest(format!("reading the request body: {inner}")).into()
}

fn multipart_error(e: axum::extract::multipart::MultipartError) -> ApiError {
    ApiError::from_rejection(e.status(), e.body_text())
}
