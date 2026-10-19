//! The cross-site check on the private interface. The private port has no
//! authentication because only trusted networks reach it; a browser on
//! that network breaks the assumption, since any page it visits can make
//! it submit a form to an internal address, and a form `POST` goes through
//! without a preflight. `PUT`, `DELETE` and JSON bodies are preflighted,
//! but `POST /blobs/:id/retry`, `finish` and `urls` take no body, and
//! neither do the dashboard's forms. So every request that is not safe is
//! checked against what the browser says about where it came from:
//! `Sec-Fetch-Site`, and on every `POST` `Origin`. A request marked as
//! another site's is refused; one that says nothing is a non-browser
//! client, the backend, and goes through, since a browser never sends a
//! `POST` without `Origin`. Stateless, no token and no session.

use axum::extract::Request;
use axum::http::header::{HOST, ORIGIN};
use axum::http::{HeaderMap, Method, Uri};
use axum::middleware::Next;
use axum::response::Response;

use crate::error::ApiError;

/// Whether the request is one the browser marked as another site's: `Err`
/// says how. `uri` supplies the authority when there is no `Host` header
/// (HTTP/2).
pub fn check(headers: &HeaderMap, uri: &Uri) -> Result<(), String> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    };
    if let Some(site) = header("sec-fetch-site") {
        return if matches!(site, "same-origin" | "none") {
            Ok(())
        } else {
            Err(format!("Sec-Fetch-Site is {site}"))
        };
    }
    let Some(origin) = header(ORIGIN.as_str()) else {
        return Ok(());
    };
    let host = header(HOST.as_str())
        .map(str::to_string)
        .or_else(|| uri.authority().map(|a| a.to_string()));
    match host {
        Some(host) if origin_matches(origin, &host) => Ok(()),
        _ => Err(format!("Origin {origin} is not this host")),
    }
}

/// Whether `method` can change anything: the check applies to it.
pub fn is_unsafe(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// A layer on the private router: `403` for an unsafe request from
/// another site.
pub async fn refuse_cross_site(req: Request, next: Next) -> Result<Response, ApiError> {
    if is_unsafe(req.method())
        && let Err(why) = check(req.headers(), req.uri())
    {
        return Err(ApiError::Forbidden(format!(
            "cross-site request refused: {why}"
        )));
    }
    Ok(next.run(req).await)
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
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| (k.parse().unwrap(), HeaderValue::from_str(v).unwrap()))
            .collect()
    }

    #[test]
    fn origin_is_compared_to_host() {
        assert!(origin_matches("http://localhost:8080", "localhost:8080"));
        assert!(origin_matches("https://Agrafe.Internal", "agrafe.internal"));
        assert!(!origin_matches("http://localhost:8080", "localhost:8081"));
        assert!(!origin_matches("null", "localhost:8080"));
        assert!(!origin_matches("http://evil.test", "localhost:8080"));
    }

    #[test]
    fn a_browser_mark_decides_and_no_mark_passes() {
        let uri: Uri = "/blobs/x/retry".parse().unwrap();
        let ok = |h: &[(&'static str, &str)]| check(&headers(h), &uri).is_ok();
        assert!(ok(&[]));
        assert!(ok(&[("sec-fetch-site", "same-origin")]));
        assert!(ok(&[("sec-fetch-site", "none")]));
        assert!(ok(&[("host", "a:1"), ("origin", "http://a:1")]));
        assert!(!ok(&[("sec-fetch-site", "cross-site")]));
        assert!(!ok(&[
            ("sec-fetch-site", "same-site"),
            ("host", "a:1"),
            ("origin", "http://a:1")
        ]));
        assert!(!ok(&[("host", "a:1"), ("origin", "http://b:1")]));
        assert!(!ok(&[("host", "a:1"), ("origin", "null")]));
        assert!(!ok(&[("origin", "http://a:1")]), "no host to match");

        let h2: Uri = "http://a:1/blobs/x/retry".parse().unwrap();
        assert!(check(&headers(&[("origin", "http://a:1")]), &h2).is_ok());
    }
}
