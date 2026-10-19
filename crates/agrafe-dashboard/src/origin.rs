//! The cross-site check on the action routes. The private port has no
//! authentication because only trusted networks reach it; a browser on
//! that network breaks the assumption, since any page it visits can make
//! it submit a form to an internal address, and a cross-origin form `POST`
//! goes through without a preflight. So an action is accepted only when
//! the browser says it came from the dashboard itself: `Sec-Fetch-Site`
//! `same-origin` (or `none`, a typed address), or, from a browser without
//! the header, an `Origin` equal to the `Host`. Stateless, no token and no
//! session. The JSON API needs none of this: `PUT`, `DELETE` and JSON
//! bodies are preflighted.

use axum::extract::FromRequestParts;
use axum::http::header::{HOST, ORIGIN};
use axum::http::request::Parts;

use crate::error::DashboardError;

/// Extracted by every action route: the request is the dashboard's own.
pub struct SameOrigin;

impl<S: Send + Sync> FromRequestParts<S> for SameOrigin {
    type Rejection = DashboardError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, DashboardError> {
        let header = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
        };
        if let Some(site) = header("sec-fetch-site") {
            return if matches!(site, "same-origin" | "none") {
                Ok(SameOrigin)
            } else {
                Err(refused(format!("Sec-Fetch-Site is {site}")))
            };
        }
        let Some(origin) = header(ORIGIN.as_str()) else {
            return Err(refused(
                "neither Sec-Fetch-Site nor Origin is set".to_string(),
            ));
        };
        let host = header(HOST.as_str())
            .map(str::to_string)
            .or_else(|| parts.uri.authority().map(|a| a.to_string()));
        match host {
            Some(host) if origin_matches(origin, &host) => Ok(SameOrigin),
            _ => Err(refused(format!("Origin {origin} is not this host"))),
        }
    }
}

fn refused(why: String) -> DashboardError {
    DashboardError::Forbidden(format!(
        "refused: the request did not come from the dashboard ({why})"
    ))
}

/// `Origin` is a scheme and an authority; it matches when the authority is
/// the `Host`, case-insensitively. `null` matches nothing.
fn origin_matches(origin: &str, host: &str) -> bool {
    origin
        .split_once("://")
        .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_is_compared_to_host() {
        assert!(origin_matches("http://localhost:8080", "localhost:8080"));
        assert!(origin_matches("https://Agrafe.Internal", "agrafe.internal"));
        assert!(!origin_matches("http://localhost:8080", "localhost:8081"));
        assert!(!origin_matches("null", "localhost:8080"));
        assert!(!origin_matches("http://evil.test", "localhost:8080"));
    }
}
