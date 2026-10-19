//! The `Host` allowlist on the private interface, against DNS rebinding.
//! The origin check (`origin`) trusts what the browser says about where a
//! request came from, and rebinding defeats it: a page on `evil.test`,
//! served on the private port's number, has its name re-pointed at the
//! private address once loaded, and from then on the browser treats
//! requests to `evil.test:PORT` as same-origin, reads the answers, and
//! can mint signed URLs for any blob. What gives the request away is
//! `Host`, which carries the rebound name. So when `server.private_hosts`
//! is set, every private request has to name one of them, whatever its
//! method, or it is a `403`. `/healthz` and `/metrics` are exempt: probes
//! and scrapers address a pod by its address, and neither route says
//! anything a rebinder wants.

use axum::extract::{Request, State};
use axum::http::header::HOST;
use axum::http::{HeaderMap, Uri};
use axum::middleware::Next;
use axum::response::Response;

use agrafe::Agrafe;

use crate::error::ApiError;

/// Routes a probe or a scraper reaches by address.
const EXEMPT: [&str; 2] = ["/healthz", "/metrics"];

/// Whether the request names one of `allowed` as its host, case-insensitively;
/// `Err` says what it named instead. An empty `allowed` allows everything.
/// `uri` supplies the authority when there is no `Host` header (HTTP/2).
pub fn check(allowed: &[String], headers: &HeaderMap, uri: &Uri) -> Result<(), String> {
    if allowed.is_empty() {
        return Ok(());
    }
    let host = headers
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .or_else(|| uri.authority().map(|a| a.as_str()))
        .unwrap_or("");
    if allowed.iter().any(|a| a.eq_ignore_ascii_case(host)) {
        Ok(())
    } else {
        Err(format!("Host {host:?} is not one of server.private_hosts"))
    }
}

/// A layer on the private router and the dashboard: `403` for a request
/// whose `Host` is not one of `server.private_hosts`.
pub async fn refuse_unknown_host(
    State(agrafe): State<Agrafe>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if !EXEMPT.contains(&req.uri().path())
        && let Err(why) = check(
            &agrafe.config().server.private_hosts,
            req.headers(),
            req.uri(),
        )
    {
        return Err(ApiError::Forbidden(format!("refused: {why}")));
    }
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn with_host(host: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(HOST, HeaderValue::from_str(host).unwrap());
        h
    }

    #[test]
    fn the_host_must_be_listed_once_a_list_is_set() {
        let uri: Uri = "/blobs".parse().unwrap();
        let allowed = vec![
            "agrafe.internal:8080".to_string(),
            "10.0.3.4:8080".to_string(),
        ];
        assert!(check(&[], &with_host("evil.test:8080"), &uri).is_ok());
        assert!(check(&allowed, &with_host("agrafe.internal:8080"), &uri).is_ok());
        assert!(check(&allowed, &with_host("Agrafe.Internal:8080"), &uri).is_ok());
        assert!(check(&allowed, &with_host("10.0.3.4:8080"), &uri).is_ok());
        assert!(check(&allowed, &with_host("evil.test:8080"), &uri).is_err());
        assert!(check(&allowed, &with_host("agrafe.internal"), &uri).is_err());
        assert!(check(&allowed, &HeaderMap::new(), &uri).is_err());

        let h2: Uri = "http://agrafe.internal:8080/blobs".parse().unwrap();
        assert!(check(&allowed, &HeaderMap::new(), &h2).is_ok());
    }
}
