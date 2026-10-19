//! The cross-site check on the action routes: `agrafe_axum::origin`, the
//! one the private router applies to every unsafe request, as an
//! extractor, so the dashboard keeps it wherever a host nests it. A
//! refused action is a `403` page.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::error::DashboardError;

/// Extracted by every action route, before anything is looked up: the
/// request is not marked as another site's.
pub struct SameOrigin;

impl<S: Send + Sync> FromRequestParts<S> for SameOrigin {
    type Rejection = DashboardError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, DashboardError> {
        agrafe_axum::origin::check(&parts.headers, &parts.uri)
            .map(|()| SameOrigin)
            .map_err(|why| {
                DashboardError::Forbidden(format!(
                    "refused: the request did not come from the dashboard ({why})"
                ))
            })
    }
}
