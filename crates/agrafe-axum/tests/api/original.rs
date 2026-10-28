//! Serving the decrypted original: headers, ranges, damaged objects.

use agrafe::jobs::JobKind;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

use crate::support::*;

#[tokio::test]
async fn get_original_full() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    // Several 1 KiB segments plus a partial one.
    let bytes = png_bytes(3500);
    let blob = ready_blob(&app, &bytes).await;
    let id = id_of(&blob);
    let private_uri = format!("/blobs/{id}/original/photo.png");

    let (status, headers, body) = fetch(&app.private, get_with(&private_uri, &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, bytes);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(headers["content-length"], "3500");
    assert_eq!(headers["accept-ranges"], "bytes");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["content-security-policy"], "sandbox");
    assert_eq!(
        headers["content-disposition"],
        "inline; filename=\"photo.png\"; filename*=UTF-8''photo%2Epng"
    );
    assert!(
        headers["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    assert!(headers.contains_key("last-modified"));
    let etag = headers["etag"].to_str().unwrap().to_string();
    assert!(etag.starts_with('"') && etag.len() == 66, "{etag}");

    // The signed public URL serves the same bytes.
    let public_uri = public_url(&blob, "original");
    let (status, _, body) = fetch(&app.public, get_with(&public_uri, &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, bytes);

    // HEAD carries the headers and no body.
    let head = Request::builder()
        .method(Method::HEAD)
        .uri(&public_uri)
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = fetch(&app.public, head).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-length"], "3500");
    assert!(body.is_empty());

    // Conditional request.
    let (status, headers, body) = fetch(
        &app.private,
        get_with(&private_uri, &[("if-none-match", &etag)]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers["etag"], etag.as_str());
    assert!(body.is_empty());

    // The filename segment must match.
    let (status, _) = call(
        &app.private,
        req(Method::GET, &format!("/blobs/{id}/original/other.png")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Signed query parameters steer the download.
    let (_, minted) = call(
        &app.private,
        json_req(
            Method::POST,
            &format!("/blobs/{id}/urls"),
            json!({"query": {"disposition": "attachment", "filename": "été.png"}}),
        ),
    )
    .await;
    let uri = public_path(minted["original"].as_str().unwrap());
    let (status, headers, _) = fetch(&app.public, get_with(&uri, &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"_t_.png\"; filename*=UTF-8''%C3%A9t%C3%A9%2Epng"
    );

    // An empty file.
    let empty = ready_blob(&app, b"").await;
    let uri = public_url(&empty, "original");
    let (status, headers, body) = fetch(&app.public, get_with(&uri, &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-length"], "0");
    assert!(body.is_empty());
    let (status, _, _) = fetch(&app.public, get_with(&uri, &[("range", "bytes=0-")])).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
}

#[tokio::test]
async fn get_original_ranges() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(3500);
    let blob = ready_blob(&app, &bytes).await;
    let uri = public_url(&blob, "original");
    let etag = {
        let (_, headers, _) = fetch(&app.public, get_with(&uri, &[])).await;
        headers["etag"].to_str().unwrap().to_string()
    };

    // Each range crosses or sits inside different 1 KiB segments (the first
    // segment holds 968 plaintext bytes, the others 1008).
    for (spec, start, end) in [
        ("bytes=0-0", 0, 1),
        ("bytes=0-967", 0, 968),
        ("bytes=967-968", 967, 969),
        ("bytes=968-1975", 968, 1976),
        ("bytes=100-3000", 100, 3001),
        ("bytes=3000-", 3000, 3500),
        ("bytes=-100", 3400, 3500),
        ("bytes=3499-3499", 3499, 3500),
        ("bytes=0-99999", 0, 3500),
    ] {
        let (status, headers, body) = fetch(&app.public, get_with(&uri, &[("range", spec)])).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT, "{spec}");
        assert_eq!(body, &bytes[start..end], "{spec}");
        assert_eq!(
            headers["content-length"],
            (end - start).to_string(),
            "{spec}"
        );
        assert_eq!(
            headers["content-range"],
            format!("bytes {start}-{}/3500", end - 1),
            "{spec}"
        );
    }

    // Unsatisfiable.
    let (status, headers, _) =
        fetch(&app.public, get_with(&uri, &[("range", "bytes=3500-")])).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers["content-range"], "bytes */3500");

    // Multiple ranges are not supported: the whole file comes back.
    let (status, _, body) = fetch(&app.public, get_with(&uri, &[("range", "bytes=0-1,5-6")])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, bytes);

    // If-Range with the current ETag honours the range; a stale one does not.
    let (status, _, _) = fetch(
        &app.public,
        get_with(&uri, &[("range", "bytes=0-1"), ("if-range", &etag)]),
    )
    .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    let (status, _, body) = fetch(
        &app.public,
        get_with(&uri, &[("range", "bytes=0-1"), ("if-range", "\"stale\"")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.len(), 3500);
}

#[tokio::test]
async fn get_original_corrupt_object() {
    let db = TestDatabase::new().await;
    use object_store::ObjectStoreExt;
    use object_store::path::Path as ObjectPath;

    let app = setup(db).await;
    let bytes = png_bytes(3500);
    let blob = ready_blob(&app, &bytes).await;
    let id = id_of(&blob);
    let uri = format!("/blobs/{id}/original/photo.png");
    let key = {
        use agrafe::blob::repo;
        let b = repo::find(app.state.pool(), parse_id(id))
            .await
            .unwrap()
            .unwrap();
        ObjectPath::from(b.storage_key.unwrap())
    };
    let stored = app
        .state
        .store()
        .get(&key)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap()
        .to_vec();

    // A flipped byte in the first segment is caught before the status line:
    // a 500 in the error envelope, not a 200 cut short.
    let mut first_bad = stored.clone();
    first_bad[agrafe_crypto::streaming::HEADER_LEN + 3] ^= 1;
    app.state.store().put(&key, first_bad.into()).await.unwrap();
    let (status, body) = call(&app.private, req(Method::GET, &uri)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"]["code"], "internal");

    // A flipped byte in the second 1 KiB segment: the first one serves, so
    // the response has started; the body then fails instead of lying.
    let mut later_bad = stored.clone();
    later_bad[1024 + 3] ^= 1;
    app.state.store().put(&key, later_bad.into()).await.unwrap();
    let res = app
        .private
        .clone()
        .oneshot(get_with(&uri, &[]))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.into_body().collect().await.is_err());
    // A range inside the intact first segment is unaffected, one inside the
    // damaged segment is a 500.
    let (status, _, body) = fetch(&app.private, get_with(&uri, &[("range", "bytes=0-99")])).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, &bytes[..100]);
    let (status, _) = call(
        &app.private,
        get_with(&uri, &[("range", "bytes=1000-1100")]),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    // Truncated inside the last segment: the intact start still serves, the
    // full body fails instead of ending short, and a range in the missing
    // tail is a 500.
    let short = stored[..stored.len() - 100].to_vec();
    app.state.store().put(&key, short.into()).await.unwrap();
    let (status, _, body) = fetch(&app.private, get_with(&uri, &[("range", "bytes=0-99")])).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, &bytes[..100]);
    let res = app
        .private
        .clone()
        .oneshot(get_with(&uri, &[]))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.into_body().collect().await.is_err());
    let (status, _) = call(&app.private, get_with(&uri, &[("range", "bytes=3400-")])).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

/// A stored blob whose row lost what reading needs is a 500, whichever
/// part is missing or mangled.
#[tokio::test]
async fn get_original_incomplete_row() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(1500);
    let blob = ready_blob(&app, &bytes).await;
    let id = id_of(&blob);
    let uri = format!("/blobs/{id}/original/photo.png");

    for (damage, repair) in [
        (
            "UPDATE blobs SET encryption_header = NULL WHERE id = $1",
            "UPDATE blobs SET encryption_header = $2 WHERE id = $1",
        ),
        (
            "UPDATE blobs SET encryption_header = '\\x00'::bytea WHERE id = $1",
            "UPDATE blobs SET encryption_header = $2 WHERE id = $1",
        ),
    ] {
        let header: Vec<u8> =
            sqlx::query_scalar("SELECT encryption_header FROM blobs WHERE id = $1")
                .bind(parse_id(id))
                .fetch_one(app.state.pool())
                .await
                .unwrap();
        sqlx::query(damage)
            .bind(parse_id(id))
            .execute(app.state.pool())
            .await
            .unwrap();
        let (status, body) = call(&app.private, req(Method::GET, &uri)).await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{damage}: {body}"
        );
        assert_eq!(body["error"]["code"], "internal");
        sqlx::query(repair)
            .bind(parse_id(id))
            .bind(header)
            .execute(app.state.pool())
            .await
            .unwrap();
        let (status, _, got) = fetch(&app.private, get_with(&uri, &[])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got, bytes);
    }

    sqlx::query("UPDATE blobs SET chunk_size = 7 WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    let (status, body) = call(&app.private, req(Method::GET, &uri)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
}

/// The bytes exist but are not served before the scan clears them.
#[tokio::test]
async fn original_waits_for_the_scan() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(3000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let (status, _) = put_private(&app, id_of(&created), bytes).await;
    assert_eq!(status, StatusCode::OK);
    let uri = public_url(&created, "original");
    let (status, body) = call(&app.public, req(Method::GET, &uri)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "conflict");
}

/// Deleted: gone from the public port, a conflict on the private one. A
/// sealed URL minted before the delete answers until the purge takes the
/// object, and no later.
#[tokio::test]
async fn original_of_a_deleted_blob() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_blob(&app, &png_bytes(3000)).await;
    let id = id_of(&blob);
    assert_eq!(delete(&app, id).await, StatusCode::NO_CONTENT);
    let sealed = public_url(&blob, "original");
    let (status, _) = call(&app.public, req(Method::GET, &sealed)).await;
    assert_eq!(status, StatusCode::OK);
    let deleted = get_blob(&app, id).await;
    assert_eq!(deleted["state"], "deleted");
    let (status, _) = call(
        &app.public,
        req(Method::GET, &public_url(&deleted, "original")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'purge'")
        .execute(app.state.pool())
        .await
        .unwrap();
    while run_job(&app, JobKind::Purge).await {}
    let (status, _) = call(&app.public, req(Method::GET, &sealed)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let uri = format!("/blobs/{id}/original/photo.png");
    let (status, _) = call(&app.private, req(Method::GET, &uri)).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// Unsigned, on the private port, a bad query is the caller's mistake.
#[tokio::test]
async fn original_rejects_a_bad_disposition() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_blob(&app, &png_bytes(3000)).await;
    let id = id_of(&blob);
    let uri = format!("/blobs/{id}/original/photo.png?disposition=weird");
    let (status, _) = call(&app.private, req(Method::GET, &uri)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// The ciphertext is bound to the object's name within its blob, not to
/// the path it sits at: moved to another layout it still reads, and under
/// another blob's name it does not.
#[tokio::test]
async fn the_original_is_bound_to_its_name_not_its_path() {
    use object_store::ObjectStoreExt;
    use object_store::path::Path as ObjectPath;

    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(3500);
    let blob = ready_blob(&app, &bytes).await;
    let id = id_of(&blob);
    let stored = app
        .state
        .store()
        .get(&ObjectPath::from(storage_key(&app, id).await))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();

    // Another layout, the same name: the row points at the copy, which
    // decrypts as it is.
    let moved = format!("elsewhere/{id}/original");
    app.state
        .store()
        .put(&ObjectPath::from(moved.as_str()), stored.clone().into())
        .await
        .unwrap();
    sqlx::query("UPDATE blobs SET storage_key = $2 WHERE id = $1")
        .bind(parse_id(id))
        .bind(&moved)
        .execute(app.state.pool())
        .await
        .unwrap();
    let uri = format!("/blobs/{id}/original/photo.png");
    let (status, _, body) = fetch(&app.private, req(Method::GET, &uri)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, bytes);

    // Another blob's name, under its own key: the same bytes, the same
    // length, and the AEAD refuses them.
    let other = ready_blob(&app, &png_bytes(3500)).await;
    let other_id = id_of(&other);
    app.state
        .store()
        .put(
            &ObjectPath::from(storage_key(&app, other_id).await),
            stored.into(),
        )
        .await
        .unwrap();
    let uri = format!("/blobs/{other_id}/original/photo.png");
    let (status, body) = call(&app.private, req(Method::GET, &uri)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"]["code"], "internal");
}
