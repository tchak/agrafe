//! API error type: every handler failure becomes a JSON body with a stable
//! code. The library's own error types map onto it here, and nowhere else.

use agrafe::blob::create::CreateError;
use agrafe::blob::urls::MintError;
use agrafe::blob::{self, representation};
use agrafe::jobs;
use agrafe::upload::UploadError;
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
    /// `404` with a reason: the thing exists but not in this form.
    #[error("{0}")]
    Missing(String),
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
    /// The server's fault: logged in full, answered as `internal error`.
    #[error("{0}")]
    Internal(Box<dyn std::error::Error + Send + Sync>),
}

impl ApiError {
    pub fn internal(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        ApiError::Internal(err.into())
    }

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
            ApiError::NotFound | ApiError::Missing(_) => StatusCode::NOT_FOUND,
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
            ApiError::NotFound | ApiError::Missing(_) => "not_found",
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
        ApiError::internal(err)
    }
}

impl From<CreateError> for ApiError {
    fn from(err: CreateError) -> Self {
        match err {
            CreateError::Invalid(msg) => ApiError::Unprocessable(msg),
            CreateError::Db(e) => ApiError::internal(e),
        }
    }
}

impl From<blob::ListError> for ApiError {
    fn from(err: blob::ListError) -> Self {
        match err {
            blob::ListError::Limit(_) | blob::ListError::ContentType(_) => {
                ApiError::BadRequest(err.to_string())
            }
            blob::ListError::Db(e) => ApiError::internal(e),
        }
    }
}

impl From<jobs::ListError> for ApiError {
    fn from(err: jobs::ListError) -> Self {
        match err {
            jobs::ListError::Limit(_) => ApiError::BadRequest(err.to_string()),
            jobs::ListError::Db(e) => ApiError::internal(e),
        }
    }
}

impl From<MintError> for ApiError {
    fn from(err: MintError) -> Self {
        match err {
            MintError::NotFound => ApiError::NotFound,
            MintError::Invalid(msg) => ApiError::Unprocessable(msg),
            MintError::Db(e) => ApiError::internal(e),
        }
    }
}

impl From<UploadError> for ApiError {
    fn from(err: UploadError) -> Self {
        match err {
            UploadError::NotFound => ApiError::NotFound,
            // The operation does not fit the blob's shape or state.
            UploadError::Conflict(_)
            | UploadError::Generated(_)
            | UploadError::NotGenerated(_)
            | UploadError::PartSequence { .. }
            | UploadError::NoParts => ApiError::Conflict(err.to_string()),
            // The rows build is a later step.
            UploadError::Unsupported(_) => ApiError::NotImplemented,
            UploadError::PartTooLarge(_) => ApiError::PayloadTooLarge(err.to_string()),
            UploadError::Undeclared
            | UploadError::SizeMismatch
            | UploadError::ChecksumMismatch
            | UploadError::TypeMismatch { .. }
            | UploadError::TooManyParts(_)
            | UploadError::Line { .. }
            | UploadError::ArchiveTooLarge(_) => ApiError::Unprocessable(err.to_string()),
            UploadError::Infected(name) => ApiError::Infected(Some(name)),
            // The body stream is ours (`body.rs`), so the box holds an
            // `ApiError` with the right status; anything else is a plain
            // bad request.
            UploadError::Body(e) => match e.downcast::<ApiError>() {
                Ok(api) => *api,
                Err(other) => ApiError::BadRequest(format!("reading the request body: {other}")),
            },
            UploadError::Store(_)
            | UploadError::Crypto(_)
            | UploadError::Key(_)
            | UploadError::Db(_)
            | UploadError::Task(_) => ApiError::internal(err),
        }
    }
}

impl From<representation::Error> for ApiError {
    fn from(err: representation::Error) -> Self {
        ApiError::internal(err)
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
                tracing::error!(error = %err, "internal error");
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
