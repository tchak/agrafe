//! Sealed URLs: the envelope in the URL is the row, and the read path
//! looks nothing up.

use agrafe::jobs::JobKind;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::json;

use crate::support::*;

#[tokio::test]
async fn sealed_urls_serve_without_the_row() {
    let app = setup().await;
    let created = create(&app, raw_blob()).await;
    assert!(
        created["envelope"].is_null(),
        "nothing to seal before ready"
    );
    assert!(!public_url(&created, "original").contains("e="));

    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    // Ready, render pending: sealed already, with the thumbnail pending in
    // the envelope.
    let before = get_blob(&app, id).await;
    assert!(before["envelope"].is_string());
    assert!(public_url(&before, "original").contains("e="));
    while run_job(&app, JobKind::Render).await {}
    let after = get_blob(&app, id).await;
    assert_ne!(
        after["envelope"], before["envelope"],
        "the render changed it"
    );

    let original = public_url(&after, "original");
    let thumbnail = public_url(&after, "thumbnail");
    let stale_thumbnail = public_url(&before, "thumbnail");
    let (status, headers, _) = fetch(&app.public, get_with(&thumbnail, &[])).await;
    assert_eq!(status, StatusCode::OK);
    let etag = headers["etag"].to_str().unwrap().to_string();
    // The stale envelope says pending: the row answers, with the picture.
    let (status, headers, _) = fetch(&app.public, get_with(&stale_thumbnail, &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["etag"], etag.as_str());

    // With the row gone, every sealed answer stands: nothing looks it up.
    sqlx::query("DELETE FROM blobs WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    let (status, headers, body) = fetch(&app.public, get_with(&original, &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/jpeg");
    assert_eq!(body, fixture("photo.jpg"));
    let (status, headers, body) = fetch(
        &app.public,
        get_with(&original, &[("range", "bytes=10-19")]),
    )
    .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["content-length"], "10");
    assert_eq!(body, fixture("photo.jpg")[10..20]);
    let (status, _, _) = fetch(&app.public, get_with(&thumbnail, &[])).await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, _) = fetch(
        &app.public,
        get_with(&thumbnail, &[("if-none-match", &etag)]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers["etag"], etag.as_str());
    let head = Request::builder()
        .method(Method::HEAD)
        .uri(&thumbnail)
        .body(Body::empty())
        .unwrap();
    let (status, headers, _) = fetch(&app.public, head).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["etag"], etag.as_str());
    // The stale one is handed to the row, which is gone.
    let (status, _, _) = fetch(&app.public, get_with(&stale_thumbnail, &[])).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // So is the metadata, which is never sealed.
    let (status, _) = call(
        &app.public,
        req(Method::GET, &public_url(&after, "metadata")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_bad_envelope_is_the_minters_mistake() {
    let app = setup().await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    // Signed, so not a forgery; it just does not decode.
    let tail = format!("/{id}/original/photo.jpg?e=bogus");
    let token = app
        .state
        .signer()
        .sign(&Method::GET, unix_now() + 3600, &tail);
    let (status, body) = call(
        &app.public,
        req(Method::GET, &format!("/blobs/{token}{tail}")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "bad_request");
    // A backend cannot ask the server to sign one in; it is the server's.
    let (status, body) = call(
        &app.private,
        json_req(
            Method::POST,
            &format!("/blobs/{id}/urls"),
            json!({"query": {"e": "x"}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}
