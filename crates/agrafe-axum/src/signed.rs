//! Signature verification extractor for the public router.

use std::time::SystemTime;

use axum::extract::{FromRef, FromRequestParts};
use http::request::Parts;

use crate::error::ApiError;
use agrafe::Agrafe;
use agrafe::signing::SignatureError;

/// Proof that the request carried a valid, unexpired signature for its exact
/// method and request target. Put it first in a handler's argument list.
#[derive(Debug, Clone, Copy)]
pub struct Signed;

impl<S> FromRequestParts<S> for Signed
where
    Agrafe: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app = Agrafe::from_ref(state);
        // The URI the route matched, with any prefix the host nested the
        // router under already stripped: the signed tail starts at `/<id>`
        // whatever that prefix is. `OriginalUri` would keep it.
        let uri = &parts.uri;

        let (token, tail) = split_target(uri.path(), uri.query())
            .ok_or_else(|| ApiError::Forbidden("invalid signature".into()))?;

        match app
            .signer()
            .verify(token, &parts.method, &tail, SystemTime::now())
        {
            Ok(()) => Ok(Signed),
            Err(SignatureError::Invalid) => Err(ApiError::Forbidden("invalid signature".into())),
            Err(SignatureError::Expired) => Err(ApiError::Gone("signature expired".into())),
            // The minter's mistake, or a cap lowered since; neither is the
            // client's business, and no config name goes to the public port.
            Err(SignatureError::TooLong) => {
                Err(ApiError::Forbidden("signature not valid yet".into()))
            }
        }
    }
}

/// `/blobs/<token>/<rest>?<query>` → `(token, "/<rest>?<query>")`.
fn split_target<'a>(path: &'a str, query: Option<&str>) -> Option<(&'a str, String)> {
    let rest = path.strip_prefix("/blobs/")?;
    let (token, after) = rest.split_once('/')?;
    if token.is_empty() || after.is_empty() {
        return None;
    }
    let mut tail = String::with_capacity(after.len() + 1 + query.map_or(0, |q| q.len() + 1));
    tail.push('/');
    tail.push_str(after);
    if let Some(q) = query {
        tail.push('?');
        tail.push_str(q);
    }
    Some((token, tail))
}

/// The request path with the token segment replaced, for logs. A token is a
/// bearer capability, so it must not land in log storage: the segment after
/// `/blobs/` goes whatever follows it, including nothing, as in a link cut
/// at its last slash, which no route answers but the span still records.
pub fn redact_token(path: &str) -> String {
    let Some(rest) = path.strip_prefix("/blobs/") else {
        return path.to_string();
    };
    let tail = rest.find('/').map_or("", |at| &rest[at..]);
    format!("/blobs/<token>{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts() {
        assert_eq!(
            redact_token("/blobs/v1.k1.1.sig/abc/original/a.jpg"),
            "/blobs/<token>/abc/original/a.jpg"
        );
        assert_eq!(redact_token("/healthz"), "/healthz");
        // A token with nothing after it is still a token.
        assert_eq!(redact_token("/blobs/v1.k1.1.sig"), "/blobs/<token>");
        assert_eq!(redact_token("/blobs/v1.k1.1.sig/"), "/blobs/<token>/");
        assert_eq!(redact_token("/blobs/"), "/blobs/<token>");
        assert_eq!(redact_token("/blobs"), "/blobs");
    }

    #[test]
    fn splits() {
        assert_eq!(
            split_target("/blobs/tok/abc/original/a%20b.jpg", Some("d=1")),
            Some(("tok", "/abc/original/a%20b.jpg?d=1".to_string()))
        );
        assert_eq!(
            split_target("/blobs/tok/abc", None),
            Some(("tok", "/abc".to_string()))
        );
        assert_eq!(split_target("/blobs/tok", None), None);
        assert_eq!(split_target("/blobs/tok/", None), None);
        assert_eq!(split_target("/other/tok/abc", None), None);
    }
}
