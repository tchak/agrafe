//! API error type: every handler failure becomes a JSON body with a stable code.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    PayloadTooLarge(String),
    #[error("{0}")]
    UnsupportedMediaType(String),
    #[error("{0}")]
    Unprocessable(String),
    /// The upload was stored and the blob is `infected`. The signature name
    /// is for the private port; the public port says only that it is.
    #[error("file is infected{}", .0.as_deref().map(|s| format!(": {s}")).unwrap_or_default())]
    Infected(Option<String>),
    #[error("not found")]
    NotFound,
    #[error("method not allowed")]
    MethodNotAllowed,
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    Gone(String),
    #[error("not implemented yet")]
    NotImplemented,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl ApiError {
    /// Convert an extractor rejection into the matching variant, keeping its
    /// status and message.
    pub fn from_rejection(status: StatusCode, message: String) -> Self {
        match status {
            StatusCode::UNPROCESSABLE_ENTITY => ApiError::Unprocessable(message),
            StatusCode::PAYLOAD_TOO_LARGE => ApiError::PayloadTooLarge(message),
            StatusCode::UNSUPPORTED_MEDIA_TYPE => ApiError::UnsupportedMediaType(message),
            StatusCode::NOT_FOUND => ApiError::NotFound,
            StatusCode::METHOD_NOT_ALLOWED => ApiError::MethodNotAllowed,
            _ => ApiError::BadRequest(message),
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::UnsupportedMediaType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ApiError::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::Infected(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            ApiError::Gone(_) => StatusCode::GONE,
            ApiError::NotImplemented => StatusCode::NOT_IMPLEMENTED,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            ApiError::BadRequest(_) => "bad_request",
            ApiError::PayloadTooLarge(_) => "payload_too_large",
            ApiError::UnsupportedMediaType(_) => "unsupported_media_type",
            ApiError::Unprocessable(_) => "unprocessable",
            ApiError::Infected(_) => "infected",
            ApiError::NotFound => "not_found",
            ApiError::MethodNotAllowed => "method_not_allowed",
            ApiError::Conflict(_) => "conflict",
            ApiError::Forbidden(_) => "forbidden",
            ApiError::Gone(_) => "gone",
            ApiError::NotImplemented => "not_implemented",
            ApiError::Internal(_) => "internal",
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        ApiError::Internal(err.into())
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}

#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'a str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let message = match &self {
            ApiError::Internal(err) => {
                tracing::error!(error = ?err, "internal error");
                "internal error".to_string()
            }
            other => other.to_string(),
        };
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code(),
                message,
            },
        };
        (status, Json(body)).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
