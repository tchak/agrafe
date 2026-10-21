//! API error type: every handler failure becomes a JSON body with a stable
//! code. The library's own error types map onto it here, and nowhere else.

use agrafe::attachment::{self, AttachError};
use agrafe::blob::create::CreateError;
use agrafe::blob::urls::MintError;
use agrafe::blob::{self, representation};
use agrafe::jobs;
use agrafe::upload::UploadError;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// A handler failure. On either port it answers its variant's status with
/// a body `{"error": {"code": ..., "message": ...}}`, where the code is
/// stable and the message, the `Display` text, is for humans.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// `400`: the request does not parse, such as malformed JSON, an
    /// unknown query value, an unreadable body or a bad filename.
    #[error("{0}")]
    BadRequest(String),
    /// `413`: the body, or one part of it, exceeds the upload size limit.
    #[error("{0}")]
    PayloadTooLarge(String),
    /// `415`: the body's `Content-Type` is not one the route accepts.
    #[error("{0}")]
    UnsupportedMediaType(String),
    /// `422`: the request parses but is invalid, such as a creation that
    /// breaks a rule or an upload whose size, checksum or type does not
    /// match what was declared.
    #[error("{0}")]
    Unprocessable(String),
    /// `422`: the upload was stored and the blob is `infected`. The signature name
    /// is for the private port; the public port says only that it is.
    #[error("file is infected{}", .0.as_deref().map(|s| format!(": {s}")).unwrap_or_default())]
    Infected(Option<String>),
    /// `404`: no such route or no such thing, or one the public port does
    /// not show.
    #[error("not found")]
    NotFound,
    /// `404` with a reason: the thing exists but not in this form.
    #[error("{0}")]
    Missing(String),
    /// `405`: the route exists, but not for this method.
    #[error("method not allowed")]
    MethodNotAllowed,
    /// `409`: the request is valid but the state of the thing forbids it,
    /// such as deleting an attached blob, uploading twice or retrying
    /// what is not dead.
    #[error("{0}")]
    Conflict(String),
    /// `403`: an invalid or not yet valid signature, a request the host or
    /// cross-site check refuses, or a public upload the blob does not
    /// allow.
    #[error("{0}")]
    Forbidden(String),
    /// `410`: an expired signature, or an expired or deleted blob that a
    /// retry or an attach cannot bring back.
    #[error("{0}")]
    Gone(String),
    /// `500`, the server's fault: logged in full, answered as `internal error`.
    #[error("{0}")]
    Internal(Box<dyn std::error::Error + Send + Sync>),
}

impl ApiError {
    /// Any error as [`ApiError::Internal`]: the one type-erased boundary,
    /// for failures the client can do nothing about.
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

    /// The HTTP status this error answers.
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
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The stable `code` of the JSON body. [`ApiError::Missing`] shares
    /// `not_found` with [`ApiError::NotFound`].
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

impl From<blob::DeleteError> for ApiError {
    fn from(err: blob::DeleteError) -> Self {
        match err {
            blob::DeleteError::Attached => ApiError::Conflict(err.to_string()),
            blob::DeleteError::Db(e) => ApiError::internal(e),
        }
    }
}

impl From<blob::RetryError> for ApiError {
    fn from(err: blob::RetryError) -> Self {
        use blob::{BlobState, RetryError};
        match err {
            RetryError::NotFound => ApiError::NotFound,
            // As for an attach: an expired or deleted blob is gone for good,
            // an infected one is still there to be told about.
            RetryError::Terminal(BlobState::Infected) => ApiError::Conflict(err.to_string()),
            RetryError::Terminal(_) => ApiError::Gone(err.to_string()),
            RetryError::Nothing(_) | RetryError::Pending => ApiError::Conflict(err.to_string()),
            RetryError::Db(e) => ApiError::internal(e),
        }
    }
}

impl From<AttachError> for ApiError {
    fn from(err: AttachError) -> Self {
        match err {
            AttachError::NotFound => ApiError::NotFound,
            AttachError::Invalid(msg) => ApiError::Unprocessable(msg),
            // An infected blob is still there, and the backend may want to
            // know; an expired or deleted one is gone for good.
            AttachError::Terminal(blob::BlobState::Infected) => ApiError::Conflict(err.to_string()),
            AttachError::Terminal(_) => ApiError::Gone(err.to_string()),
            AttachError::Db(e) => ApiError::internal(e),
        }
    }
}

impl From<attachment::ListError> for ApiError {
    fn from(err: attachment::ListError) -> Self {
        match err {
            attachment::ListError::Limit(_) | attachment::ListError::OwnerIdAlone => {
                ApiError::BadRequest(err.to_string())
            }
            attachment::ListError::Db(e) => ApiError::internal(e),
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
            | UploadError::Fetched(_)
            | UploadError::NotGenerated(_)
            | UploadError::PartSequence { .. }
            | UploadError::NoParts => ApiError::Conflict(err.to_string()),
            UploadError::PartTooLarge(_) => ApiError::PayloadTooLarge(err.to_string()),
            UploadError::Undeclared
            | UploadError::SizeMismatch
            | UploadError::BodyTooLarge(_)
            | UploadError::ChecksumMismatch
            | UploadError::Md5Mismatch
            | UploadError::TypeMismatch { .. }
            | UploadError::TypeNotAccepted(_)
            | UploadError::TooManyParts(_)
            | UploadError::Line { .. }
            | UploadError::ArchiveTooLarge(_)
            | UploadError::WorkspaceTooLarge(_)
            | UploadError::TooManyFiles(_) => ApiError::Unprocessable(err.to_string()),
            UploadError::Infected(name) => ApiError::Infected(Some(name)),
            // The body stream is ours (`body.rs`), so the box holds an
            // `ApiError` with the right status; anything else is a plain
            // bad request.
            UploadError::Body(e) => match e.downcast::<ApiError>() {
                Ok(api) => *api,
                Err(other) => ApiError::BadRequest(format!("reading the request body: {other}")),
            },
            UploadError::NoColumns
            | UploadError::Store(_)
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

/// What a handler returns: its response, or an [`ApiError`].
pub type ApiResult<T> = Result<T, ApiError>;
