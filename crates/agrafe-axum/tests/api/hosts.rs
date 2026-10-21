//! The `Host` allowlist of the private interface.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};

use crate::support::*;

fn with_host(method: Method, uri: &str, host: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, host)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn unset_the_private_port_answers_any_host() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let (status, _) = call(
        &app.private,
        with_host(Method::GET, "/blobs", "evil.test:8080"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn set_a_private_request_must_name_a_listed_host() {
    let db = TestDatabase::new().await;
    let app = setup_tuned(db, fake_clamd().await, |c| {
        c.server.private_hosts = vec!["agrafe.internal:8080".into(), "10.0.3.4:8080".into()];
    })
    .await;
    for host in [
        "agrafe.internal:8080",
        "AGRAFE.internal:8080",
        "10.0.3.4:8080",
    ] {
        let (status, body) = call(&app.private, with_host(Method::GET, "/blobs", host)).await;
        assert_eq!(status, StatusCode::OK, "{host}: {body}");
    }
    // The rebound name, on a read and on a write, and a missing Host.
    for (method, uri) in [(Method::GET, "/blobs"), (Method::POST, "/blobs/x/retry")] {
        let (status, body) = call(
            &app.private,
            with_host(method.clone(), uri, "evil.test:8080"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {body}");
        assert_eq!(body["error"]["code"], "forbidden", "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("private_hosts"),
            "{body}"
        );
    }
    let (status, _) = call(&app.private, req(Method::GET, "/blobs")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no Host at all");
    // Probes and scrapers address the pod, not the service.
    for uri in ["/healthz", "/metrics"] {
        let (status, body) = call(&app.private, with_host(Method::GET, uri, "10.1.2.3:8080")).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }
    // The public port is not concerned: its URLs are signed.
    let (status, _) = call(
        &app.public,
        with_host(Method::GET, "/healthz", "evil.test:8081"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
