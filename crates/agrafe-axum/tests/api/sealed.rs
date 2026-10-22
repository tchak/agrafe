//! Sealed URLs: the envelope in the URL is the row, and the read path
//! looks nothing up.

use agrafe::Agrafe;
use agrafe::config::Base64Secret;
use agrafe::jobs::JobKind;
use agrafe_axum as api;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;

use crate::support::*;

#[tokio::test]
async fn sealed_urls_serve_without_the_row() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
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

    // Expired after minting: the sealed answers stand until the purge takes
    // the objects, while the row-backed routes hide the blob at once.
    sqlx::query("UPDATE blobs SET expires_at = now() - interval '1 hour' WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    let (status, _, body) = fetch(&app.public, get_with(&original, &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, fixture("photo.jpg"));
    let (status, _, _) = fetch(&app.public, get_with(&thumbnail, &[])).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &app.public,
        req(Method::GET, &public_url(&after, "metadata")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "expired");

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
    let db = TestDatabase::new().await;
    let app = setup(db).await;
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
    // Another blob's envelope, signed in: its wrapped key is bound to that
    // blob's id, so it does not open here, and that too is the minter's.
    let other = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    while run_job(&app, JobKind::Render).await {}
    let other = get_blob(&app, id_of(&other)).await;
    let envelope = other["envelope"].as_str().unwrap();
    for kind in ["original", "thumbnail"] {
        let tail = format!("/{id}/{kind}/photo.jpg?e={envelope}");
        let token = app
            .state
            .signer()
            .sign(&Method::GET, unix_now() + 3600, &tail);
        let (status, body) = call(
            &app.public,
            req(Method::GET, &format!("/blobs/{token}{tail}")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{kind}: {body}");
        assert_eq!(body["error"]["code"], "bad_request");
    }
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

/// The same database and bucket under another `[encryption]`: `tune`
/// edits the KEKs, and the handle and its routers come back.
fn rotated(
    app: &TestApp,
    tune: impl FnOnce(&mut agrafe::config::EncryptionConfig),
) -> (Agrafe, Router, Router) {
    let mut config = app.state.config().clone();
    tune(&mut config.encryption);
    let store = agrafe::storage::build(&config.storage).unwrap();
    let state = Agrafe::with_store(config, app.state.pool().clone(), store).unwrap();
    let private = api::private_router(state.clone());
    let public = api::public_router(state.clone());
    (state, private, public)
}

/// The row's data key wrapped again under `state`'s current KEK, as the
/// rewrap that retires a KEK would do it.
async fn rewrap(app: &TestApp, state: &Agrafe, id: &str) {
    let (kek_id, wrapped): (String, Vec<u8>) =
        sqlx::query_as("SELECT kek_id, wrapped_key FROM blobs WHERE id = $1")
            .bind(parse_id(id))
            .fetch_one(app.state.pool())
            .await
            .unwrap();
    let (kek_id, wrapped) = state
        .keks()
        .rewrap(parse_id(id), &kek_id, &wrapped)
        .unwrap();
    sqlx::query("UPDATE blobs SET kek_id = $2, wrapped_key = $3 WHERE id = $1")
        .bind(parse_id(id))
        .bind(kek_id)
        .bind(wrapped)
        .execute(app.state.pool())
        .await
        .unwrap();
}

/// A blob uploaded through `private`, rendered by `app`, and its metadata.
async fn ready_through(app: &TestApp, private: &Router, bytes: &[u8]) -> serde_json::Value {
    let mut blob = blob_for(bytes, "image/jpeg", "raw");
    blob["filename"] = json!("photo.jpg");
    blob["skip_virus_scan"] = json!(true);
    let (status, created) = call(private, json_req(Method::POST, "/blobs", blob)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = id_of(&created).to_string();
    let (status, body) = call(private, put_raw(&format!("/blobs/{id}"), bytes.to_vec())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    while run_job(app, JobKind::Render).await {}
    let (status, blob) = call(private, req(Method::GET, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::OK, "{blob}");
    blob
}

async fn kek_of(app: &TestApp, id: &str) -> String {
    sqlx::query_scalar("SELECT kek_id FROM blobs WHERE id = $1")
        .bind(parse_id(id))
        .fetch_one(app.state.pool())
        .await
        .unwrap()
}

/// The row path, a signed URL without an envelope, for a blob whose
/// minted URLs are all sealed.
fn row_url(app: &TestApp, id: &str) -> String {
    let tail = format!("/{id}/original/photo.jpg");
    let token = app
        .state
        .signer()
        .sign(&Method::GET, unix_now() + 3600, &tail);
    format!("/blobs/{token}{tail}")
}

#[tokio::test]
async fn kek_rotation_end_to_end() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let kek2 = Base64Secret::from(STANDARD.encode([44u8; 32]));
    let old = ready_through(&app, &app.private, &fixture("photo.jpg")).await;
    assert_eq!(kek_of(&app, id_of(&old)).await, "kek1");

    // A new current KEK, the old one kept: new blobs wrap under the new
    // one, and both read, sealed or from the row.
    let (state, private, public) = rotated(&app, |e| {
        e.keks.insert("kek2".into(), kek2.clone());
        e.current_kek = Some("kek2".into());
    });
    let new = ready_through(&app, &private, &fixture("photo.jpg")).await;
    assert_eq!(kek_of(&app, id_of(&new)).await, "kek2");
    for blob in [&old, &new] {
        for url in [public_url(blob, "original"), row_url(&app, id_of(blob))] {
            let (status, _, body) = fetch(&public, get_with(&url, &[])).await;
            assert_eq!(status, StatusCode::OK, "{url}");
            assert_eq!(body, fixture("photo.jpg"));
        }
        let (status, _, _) = fetch(&public, get_with(&public_url(blob, "thumbnail"), &[])).await;
        assert_eq!(status, StatusCode::OK);
    }

    // A process that never learnt the new KEK: an envelope naming a KEK it
    // does not know is handed to the row, which does not unwrap either, a
    // logged failure on both paths rather than a 400 the minter cannot act on.
    for url in [public_url(&new, "original"), row_url(&app, id_of(&new))] {
        let (status, body) = call(&app.public, req(Method::GET, &url)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{url}: {body}");
    }

    // The old KEK retired before its blobs were rewrapped: the same, for
    // the old blob.
    let (_, _, retired) = rotated(&app, |e| {
        e.keks.clear();
        e.keks.insert("kek2".into(), kek2.clone());
        e.current_kek = Some("kek2".into());
    });
    let (status, _, _) = fetch(&retired, get_with(&public_url(&new, "original"), &[])).await;
    assert_eq!(status, StatusCode::OK);
    for url in [
        public_url(&old, "original"),
        public_url(&old, "thumbnail"),
        row_url(&app, id_of(&old)),
    ] {
        let (status, body) = call(&retired, req(Method::GET, &url)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{url}: {body}");
    }

    // Rewrapped under the new KEK, then the old one retired: the envelopes
    // the backend stored still name the old KEK, and are served from the
    // row until it refreshes them. Nothing breaks when the old KEK goes.
    rewrap(&app, &state, id_of(&old)).await;
    assert_eq!(kek_of(&app, id_of(&old)).await, "kek2");
    let fresh = get_blob(&app, id_of(&old)).await;
    assert_ne!(fresh["envelope"], old["envelope"], "the rewrap changed it");
    for url in [
        public_url(&old, "original"),
        public_url(&fresh, "original"),
        row_url(&app, id_of(&old)),
    ] {
        let (status, _, body) = fetch(&retired, get_with(&url, &[])).await;
        assert_eq!(status, StatusCode::OK, "{url}");
        assert_eq!(body, fixture("photo.jpg"));
    }
    for blob in [&old, &fresh] {
        let (status, _, _) = fetch(&retired, get_with(&public_url(blob, "thumbnail"), &[])).await;
        assert_eq!(status, StatusCode::OK);
    }
    // The stale envelope is only as good as the row: once that is gone,
    // so is the blob, where the fresh one still serves from the bucket.
    sqlx::query("DELETE FROM blobs WHERE id = $1")
        .bind(parse_id(id_of(&old)))
        .execute(app.state.pool())
        .await
        .unwrap();
    let (status, _) = call(&retired, req(Method::GET, &public_url(&old, "original"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = fetch(&retired, get_with(&public_url(&fresh, "original"), &[])).await;
    assert_eq!(status, StatusCode::OK);
}
