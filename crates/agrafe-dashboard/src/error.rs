//! What a page can fail with, rendered as an HTML page rather than the
//! JSON body of the API. The library's error enums map onto it here.

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use agrafe::{attachment, blob, jobs};

use crate::layout::{Base, ErrorPage, Layout};

#[derive(Debug, thiserror::Error)]
pub enum DashboardError {
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    BadRequest(String),
    /// A state-changing request from another origin, see `origin`.
    #[error("{0}")]
    Forbidden(String),
    /// The server's fault: logged in full, shown as `internal error`.
    #[error("{0}")]
    Internal(Box<dyn std::error::Error + Send + Sync>),
}

impl DashboardError {
    pub fn internal(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        DashboardError::Internal(err.into())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            DashboardError::NotFound => StatusCode::NOT_FOUND,
            DashboardError::BadRequest(_) => StatusCode::BAD_REQUEST,
            DashboardError::Forbidden(_) => StatusCode::FORBIDDEN,
            DashboardError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<sqlx::Error> for DashboardError {
    fn from(err: sqlx::Error) -> Self {
        DashboardError::internal(err)
    }
}

impl From<jobs::ListError> for DashboardError {
    fn from(err: jobs::ListError) -> Self {
        match err {
            jobs::ListError::Limit(_) | jobs::ListError::Cursor => {
                DashboardError::BadRequest(err.to_string())
            }
            jobs::ListError::Db(e) => DashboardError::internal(e),
        }
    }
}

impl From<blob::ListError> for DashboardError {
    fn from(err: blob::ListError) -> Self {
        match err {
            blob::ListError::Limit(_) | blob::ListError::ContentType(_) => {
                DashboardError::BadRequest(err.to_string())
            }
            blob::ListError::Db(e) => DashboardError::internal(e),
        }
    }
}

impl From<attachment::ListError> for DashboardError {
    fn from(err: attachment::ListError) -> Self {
        match err {
            attachment::ListError::Limit(_)
            | attachment::ListError::OwnerIdAlone
            | attachment::ListError::RoleAlone => DashboardError::BadRequest(err.to_string()),
            attachment::ListError::Db(e) => DashboardError::internal(e),
        }
    }
}

/// What an error leaves on its bare response for [`render`] to turn into
/// a page: the page needs the mount point, which the error does not know.
#[derive(Debug, Clone)]
pub struct Failure {
    pub status: StatusCode,
    pub message: String,
}

impl IntoResponse for DashboardError {
    fn into_response(self) -> Response {
        let status = self.status();
        let message = match &self {
            DashboardError::Internal(err) => {
                tracing::error!(error = %err, "internal error");
                "internal error".to_string()
            }
            other => other.to_string(),
        };
        let mut res = status.into_response();
        res.extensions_mut().insert(Failure { status, message });
        res
    }
}

/// The error page, for a response that carries a [`Failure`]. A layer on
/// every route and the fallback, so a bad id, an unknown path and a
/// refused action all come back as the same page.
pub async fn render(base: Base, req: Request, next: Next) -> Response {
    let res = next.run(req).await;
    let Some(failure) = res.extensions().get::<Failure>().cloned() else {
        return res;
    };
    let page = ErrorPage {
        layout: Layout::new(&base, ""),
        status: failure.status.as_u16(),
        reason: failure.status.canonical_reason().unwrap_or("Error"),
        message: failure.message,
    };
    (failure.status, page).into_response()
}
