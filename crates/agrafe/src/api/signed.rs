//! Signature verification extractor for the public router.

use std::time::SystemTime;

use axum::extract::{FromRef, FromRequestParts, OriginalUri};
use http::request::Parts;

use crate::app::AppState;
use crate::error::ApiError;
use crate::signing::SignatureError;

/// Proof that the request carried a valid, unexpired signature for its exact
/// method and request target. Put it first in a handler's argument list.
#[derive(Debug, Clone)]
pub struct Signed {
    pub token: String,
}

impl<S> FromRequestParts<S> for Signed
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app = AppState::from_ref(state);
        let uri = parts
            .extensions
            .get::<OriginalUri>()
            .map(|o| o.0.clone())
            .unwrap_or_else(|| parts.uri.clone());

        let (token, tail) = split_target(uri.path(), uri.query())
            .ok_or_else(|| ApiError::Forbidden("invalid signature".into()))?;

        match app
            .signer()
            .verify(token, &parts.method, &tail, SystemTime::now())
        {
            Ok(()) => Ok(Signed {
                token: token.to_string(),
            }),
            Err(SignatureError::Invalid) => Err(ApiError::Forbidden("invalid signature".into())),
            Err(SignatureError::Expired) => Err(ApiError::Gone("signature expired".into())),
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

#[cfg(test)]
mod tests {
    use super::*;

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
