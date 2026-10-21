//! CORS on the public interface, `server.cors`.

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};

use crate::support::*;

const APP: &str = "https://app.example";

fn preflight(uri: &str, origin: &str, method: &str) -> Request<Body> {
    Request::builder()
        .method(Method::OPTIONS)
        .uri(uri)
        .header("origin", origin)
        .header("access-control-request-method", method)
        .header("access-control-request-headers", "content-type")
        .body(Body::empty())
        .unwrap()
}

fn from(origin: &str, mut req: Request<Body>) -> Request<Body> {
    req.headers_mut().insert("origin", origin.parse().unwrap());
    req
}

fn allowed_origin(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("access-control-allow-origin")
        .map(|v| v.to_str().unwrap())
}

async fn setup_cors(db: TestDatabase, origins: &[&str]) -> TestApp {
    let origins = origins.iter().map(|o| o.to_string()).collect();
    setup_tuned(db, fake_clamd().await, |c| {
        c.server.cors.allowed_origins = origins;
    })
    .await
}

#[tokio::test]
async fn off_by_default_no_headers_and_no_preflight() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_blob(&app, &png_bytes(500)).await;
    let original = public_url(&blob, "original");

    let (status, headers, _) = fetch(&app.public, preflight(&original, APP, "GET")).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(allowed_origin(&headers), None);

    let (status, headers, _) = fetch(&app.public, from(APP, get_with(&original, &[]))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(allowed_origin(&headers), None);
}

/// The browser's whole exchange for a direct upload from another origin:
/// an unsigned preflight, then the signed `PUT`, then a read.
#[tokio::test]
async fn an_allowed_origin_uploads_and_reads() {
    let db = TestDatabase::new().await;
    let app = setup_cors(db, &[APP, "http://localhost:3000"]).await;
    let bytes = png_bytes(500);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let upload = public_url(&created, "upload");

    // The preflight carries no signature for OPTIONS, and needs none.
    let (status, headers, body) = fetch(&app.public, preflight(&upload, APP, "PUT")).await;
    assert!(status.is_success(), "{status}");
    assert!(body.is_empty());
    assert_eq!(allowed_origin(&headers), Some(APP));
    assert!(
        headers["access-control-allow-methods"]
            .to_str()
            .unwrap()
            .contains("PUT")
    );
    assert!(
        headers["access-control-allow-headers"]
            .to_str()
            .unwrap()
            .contains("content-type")
    );
    assert_eq!(headers["access-control-max-age"], "3600");
    assert!(headers["vary"].to_str().unwrap().contains("origin"));
    assert!(!headers.contains_key("access-control-allow-credentials"));
    assert_eq!(get_blob(&app, id_of(&created)).await["state"], "created");

    let (status, headers, _) = fetch(&app.public, from(APP, put_raw(&upload, bytes))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(allowed_origin(&headers), Some(APP));

    let ready = get_blob(&app, id_of(&created)).await;
    let original = public_url(&ready, "original");
    let (status, headers, _) = fetch(
        &app.public,
        from("http://localhost:3000", get_with(&original, &[])),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(allowed_origin(&headers), Some("http://localhost:3000"));
    let exposed = headers["access-control-expose-headers"].to_str().unwrap();
    for name in ["content-disposition", "content-range", "etag"] {
        assert!(exposed.contains(name), "{exposed}");
    }
}

#[tokio::test]
async fn another_origin_gets_no_headers() {
    let db = TestDatabase::new().await;
    let app = setup_cors(db, &[APP]).await;
    let blob = ready_blob(&app, &png_bytes(500)).await;
    let original = public_url(&blob, "original");

    for origin in ["https://evil.example", "https://app.example.evil.example"] {
        let (_, headers, _) = fetch(&app.public, preflight(&original, origin, "GET")).await;
        assert_eq!(allowed_origin(&headers), None, "{origin}");
        let (status, headers, _) = fetch(&app.public, from(origin, get_with(&original, &[]))).await;
        // The browser withholds the response; the server serves it as to curl.
        assert_eq!(status, StatusCode::OK);
        assert_eq!(allowed_origin(&headers), None, "{origin}");
    }
}

/// A page reads why a request failed, not a bare network error.
#[tokio::test]
async fn errors_carry_the_headers() {
    let db = TestDatabase::new().await;
    let app = setup_cors(db, &[APP]).await;
    let blob = ready_blob(&app, &png_bytes(500)).await;
    let forged = public_url(&blob, "original").replacen("/blobs/v1.", "/blobs/v2.", 1);

    let (status, headers, body) = fetch(&app.public, from(APP, get_with(&forged, &[]))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(allowed_origin(&headers), Some(APP));
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "forbidden", "{body}");

    let (status, headers, _) = fetch(&app.public, from(APP, get_with("/nowhere", &[]))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(allowed_origin(&headers), Some(APP));
}

#[tokio::test]
async fn a_wildcard_allows_any_origin() {
    let db = TestDatabase::new().await;
    let app = setup_cors(db, &["*"]).await;
    let blob = ready_blob(&app, &png_bytes(500)).await;
    let original = public_url(&blob, "original");

    let (status, headers, _) = fetch(
        &app.public,
        preflight(&original, "https://any.example", "GET"),
    )
    .await;
    assert!(status.is_success(), "{status}");
    assert_eq!(allowed_origin(&headers), Some("*"));
    let (_, headers, _) = fetch(
        &app.public,
        from("https://any.example", get_with(&original, &[])),
    )
    .await;
    assert_eq!(allowed_origin(&headers), Some("*"));
    assert!(!headers.contains_key("access-control-allow-credentials"));
}

/// The private port is for the backend, never a page: no CORS there.
#[tokio::test]
async fn the_private_port_sends_none() {
    let db = TestDatabase::new().await;
    let app = setup_cors(db, &["*"]).await;
    let (status, headers, _) = fetch(&app.private, from(APP, get_with("/blobs", &[]))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(allowed_origin(&headers), None);
}
