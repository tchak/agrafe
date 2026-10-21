//! The upload pipeline: raw and multipart bodies, the checks on them,
//! and what is left behind when an upload fails.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures::StreamExt as _;
use md5::{Digest, Md5};
use serde_json::Value;

use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;
use tower::ServiceExt;

use crate::support::*;

#[tokio::test]
async fn raw_upload_private() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    // Longer than the sniff window, so the type is checked mid-stream.
    let bytes = png_bytes(10 * 1024);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);

    let (status, body) = put_private(&app, id, bytes.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The scan is deferred, so the blob waits for it.
    assert_eq!(body["state"], "uploaded");
    assert_eq!(body["scan_state"], "pending");
    assert!(body["uploaded_at"].is_string());
    assert_eq!(jobs_for(&app, id).await, vec!["scan"]);
    assert_eq!(stored_plaintext(&app, id).await, bytes);
    assert_eq!(
        created["checksum_md5"],
        Value::Null,
        "the MD5 is the service's to compute"
    );
    assert_eq!(
        body["checksum_md5"],
        STANDARD.encode(Md5::digest(&bytes)),
        "Active Storage's checksum of the bytes"
    );

    // Write-once: a second upload is refused.
    let (status, _) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// The backend vouches for the source: ready at once, representations queued.
#[tokio::test]
async fn vouched_upload_is_ready_at_once() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let body = ready_blob(&app, &png_bytes(5000)).await;
    assert_eq!(body["state"], "ready");
    assert_eq!(body["scan_state"], "skipped");
    assert_eq!(jobs_for(&app, id_of(&body)).await, vec!["render"]);
}

/// An empty file is a valid file.
#[tokio::test]
async fn empty_file_upload() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, blob_for(b"", "text/plain", "raw")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, vec![]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(stored_plaintext(&app, id).await, b"");
}

#[tokio::test]
async fn raw_upload_rejections() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(3000);
    let mut flipped = bytes.clone();
    flipped[1500] ^= 0xff;
    let longer = [bytes.as_slice(), &[0]].concat();

    // Each failure answers 422 naming what is wrong, hands the blob back to
    // `created` and stores nothing. Retrying with the right bytes then works,
    // unless what was declared is itself wrong.
    let cases = [
        // Same length as declared, one byte off: only the hash can tell.
        ("checksum", "image/png", flipped, "checksum", StatusCode::OK),
        (
            "too short",
            "image/png",
            bytes[..2999].to_vec(),
            "size",
            StatusCode::OK,
        ),
        ("too long", "image/png", longer, "size", StatusCode::OK),
        (
            "type",
            "image/jpeg",
            bytes.clone(),
            "looks like",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ];
    for (name, content_type, body, message, retry) in cases {
        let created = create(&app, blob_for(&bytes, content_type, "raw")).await;
        let id = id_of(&created);
        let (status, res) = put_private(&app, id, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {res}");
        let said = res["error"]["message"].as_str().unwrap();
        assert!(said.contains(message), "{name}: {res}");
        let got = get_blob(&app, id).await;
        assert_eq!(got["state"], "created", "{name}");
        assert!(got["uploaded_at"].is_null(), "{name}");
        assert!(jobs_for(&app, id).await.is_empty(), "{name}");

        let (status, res) = put_private(&app, id, bytes.clone()).await;
        assert_eq!(status, retry, "{name}: {res}");
    }
    // Only the retries that went through stored anything.
    let objects = stored_objects(&app).await;
    assert_eq!(objects.len(), 3, "{objects:?}");
}

/// A Content-Length that disagrees with the declaration is refused before
/// the body is read.
#[tokio::test]
async fn content_length_mismatch_is_refused_up_front() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(3000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let mismatched = Request::builder()
        .method(Method::PUT)
        .uri(format!("/blobs/{id}"))
        .header(header::CONTENT_LENGTH, bytes.len() - 1)
        .body(Body::from(bytes))
        .unwrap();
    let (status, res) = call(&app.private, mismatched).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{res}");
    let said = res["error"]["message"].as_str().unwrap();
    assert!(said.contains("Content-Length"), "{res}");
    assert_eq!(get_blob(&app, id).await["state"], "created");
}

/// Past the sniff window the type is checked while the body still arrives.
/// The body here never ends, so the upload can only stop at that check, and
/// the message says so: checked at the end, it would be a size mismatch.
#[tokio::test]
async fn type_mismatch_past_the_sniff_window() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(10 * 1024);
    let head = agrafe::sniff::SNIFF_LEN + 512;
    assert!(head < bytes.len());
    let created = create(&app, blob_for(&bytes, "image/jpeg", "raw")).await;
    let id = id_of(&created);

    let pieces: Vec<Result<bytes::Bytes, std::io::Error>> = bytes[..head]
        .chunks(512)
        .map(|p| Ok(bytes::Bytes::copy_from_slice(p)))
        .collect();
    let endless = futures::stream::iter(pieces).chain(futures::stream::repeat_with(|| {
        Ok(bytes::Bytes::from_static(&[0; 512]))
    }));
    let put = Request::builder()
        .method(Method::PUT)
        .uri(format!("/blobs/{id}"))
        .body(Body::from_stream(endless))
        .unwrap();
    let (status, body) = tokio::time::timeout(Duration::from_secs(5), call(&app.private, put))
        .await
        .expect("the upload stops at the type check");
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("looks like `image/png`"), "{body}");

    assert_eq!(get_blob(&app, id).await["state"], "created");
    let objects = stored_objects(&app).await;
    assert!(objects.is_empty(), "{objects:?}");
}

/// The client's body fails partway: a 400, the blob handed back, nothing
/// stored.
#[tokio::test]
async fn body_error_midway() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(5000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);

    let half = bytes::Bytes::copy_from_slice(&bytes[..2500]);
    let failing = futures::stream::iter([
        Ok(half),
        Err(std::io::Error::other("connection reset by peer")),
    ]);
    let put = Request::builder()
        .method(Method::PUT)
        .uri(format!("/blobs/{id}"))
        .body(Body::from_stream(failing))
        .unwrap();
    let (status, body) = call(&app.private, put).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "bad_request");
    assert_eq!(get_blob(&app, id).await["state"], "created");
    let objects = stored_objects(&app).await;
    assert!(objects.is_empty(), "{objects:?}");
}

#[tokio::test]
async fn multipart_upload_public() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(2500);
    let created = create(&app, blob_for(&bytes, "image/png", "multipart")).await;
    let id = id_of(&created);

    // Extra parts are ignored; the one named `file` is the file.
    let (status, body) = call(
        &app.public,
        put_multipart(
            &public_url(&created, "upload"),
            &[("comment", b"hello"), ("file", &bytes), ("after", b"x")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert!(body.get("filename").is_none(), "public response is reduced");
    assert_eq!(stored_plaintext(&app, id).await, bytes);
}

/// Every bad multipart body leaves the blob in `created`.
#[tokio::test]
async fn multipart_rejections() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(2500);
    let created = create(&app, blob_for(&bytes, "image/png", "multipart")).await;
    let id = id_of(&created);
    let upload = public_url(&created, "upload");

    // No `file` part.
    let (status, body) = call(&app.public, put_multipart(&upload, &[("other", &bytes)])).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(get_blob(&app, id).await["state"], "created");

    // Not a multipart body at all.
    let (status, body) = call(&app.public, put_raw(&upload, bytes.clone())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // The route body limit is `upload.max_size` plus 1 MiB of slack; a body
    // that blows through it on the way to the `file` part is a 413.
    let junk = vec![b'x'; 1024 * 1024 + 32 * 1024];
    let (status, body) = call(
        &app.public,
        put_multipart(&upload, &[("comment", &junk), ("file", &bytes)]),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"]["code"], "payload_too_large");
    assert_eq!(get_blob(&app, id).await["state"], "created");
}

#[tokio::test]
async fn raw_upload_public() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(2500);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let upload = public_url(&created, "upload");
    let (status, body) = call(&app.public, put_raw(&upload, bytes)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn upload_url_is_dead_after_one_success() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(1500);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let upload = public_url(&created, "upload");

    let (status, body) = call(&app.public, put_raw(&upload, bytes.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "ready", "small enough to scan inline");

    // Blobs are write-once: the same URL, leaked or replayed, earns a 409 on
    // either port and the stored bytes are untouched.
    let other = png_bytes(4000);
    let (status, body) = call(&app.public, put_raw(&upload, other.clone())).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = put_private(&app, id, other).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(stored_plaintext(&app, id).await, bytes);
    assert_eq!(
        storage_key(&app, id).await,
        format!("{id}/original"),
        "one object per blob, no version in the key"
    );
}

#[tokio::test]
async fn dropped_upload_is_aborted() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(5000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let uri = format!("/blobs/{id}");

    // Half the file, then a body that never ends, in a handler future that is
    // dropped while it waits: what an HTTP/2 stream reset or a closed
    // connection does to a handler.
    let half = bytes::Bytes::from(bytes[..2500].to_vec());
    let body = Body::from_stream(
        futures::stream::once(async move { Ok::<_, std::io::Error>(half) })
            .chain(futures::stream::pending()),
    );
    let hanging = Request::builder()
        .method(Method::PUT)
        .uri(&uri)
        .body(body)
        .unwrap();
    let outcome = tokio::time::timeout(
        Duration::from_millis(200),
        app.private.clone().oneshot(hanging),
    )
    .await;
    assert!(
        outcome.is_err(),
        "the handler should still be reading the body"
    );

    // The pipeline task sees the body end early and hands the blob back.
    wait_for_state(&app, id, "created").await;
    let objects = stored_objects(&app).await;
    assert!(objects.is_empty(), "{objects:?}");

    // And the upload can be retried.
    let (status, body) = call(&app.private, put_raw(&uri, bytes.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Deleted while its bytes are still arriving: the upload loses, and the
/// object it wrote is removed rather than left to nobody. Nothing was
/// stored when the delete ran, so no purge job would ever find it.
#[tokio::test]
async fn delete_during_upload() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(5000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(1);
    let put = Request::builder()
        .method(Method::PUT)
        .uri(format!("/blobs/{id}"))
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .unwrap();
    let router = app.private.clone();
    let upload = tokio::spawn(async move { call(&router, put).await });

    tx.send(Ok(bytes::Bytes::copy_from_slice(&bytes[..2500])))
        .await
        .unwrap();
    wait_for_state(&app, id, "uploading").await;
    assert_eq!(delete(&app, id).await, StatusCode::NO_CONTENT);
    tx.send(Ok(bytes::Bytes::copy_from_slice(&bytes[2500..])))
        .await
        .unwrap();
    drop(tx);

    let (status, body) = within("the upload", upload).await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("deleted"),
        "{body}"
    );
    assert_eq!(get_blob(&app, id).await["state"], "deleted");
    let objects = stored_objects(&app).await;
    assert!(objects.is_empty(), "{objects:?}");
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(app.state.pool())
        .await
        .unwrap();
    assert_eq!(jobs, 0);
}
