//! Thumbnails and previews: rendering from images, PDFs, office and text
//! files, the placeholders served meanwhile, waiting, and regeneration.

use agrafe::jobs::JobKind;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use object_store::ObjectStoreExt;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

use crate::support::*;

/// Run the render job for every blob that has one queued.
async fn render_all(app: &TestApp) {
    while run_job(app, JobKind::Render).await {}
}

fn state_of(headers: &HeaderMap) -> &str {
    headers["x-representation-state"].to_str().unwrap()
}

fn cache_of(headers: &HeaderMap) -> &str {
    headers["cache-control"].to_str().unwrap()
}

/// A rendered representation: `200`, WebP, cacheable, with its size.
async fn rendered(app: &TestApp, uri: &str) -> (HeaderMap, (u32, u32)) {
    let (status, headers, body) = fetch(&app.private, get_with(uri, &[])).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    assert_eq!(headers["content-type"], "image/webp");
    assert_eq!(state_of(&headers), "ready");
    assert_eq!(cache_of(&headers), "private, max-age=86400");
    assert_eq!(headers["content-length"], body.len().to_string().as_str());
    let size = webp_size(&body);
    (headers, size)
}

/// The icon served in place of a thumbnail, in the given state.
async fn icon(app: &TestApp, uri: &str, state: &str) -> Vec<u8> {
    let (status, headers, body) = fetch(&app.private, get_with(uri, &[])).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    assert_eq!(headers["content-type"], "image/webp");
    assert_eq!(state_of(&headers), state);
    let cache = if state == "pending" {
        "no-store"
    } else {
        "private, max-age=86400"
    };
    assert_eq!(cache_of(&headers), cache, "{uri}");
    assert!(!headers.contains_key("etag"));
    assert_eq!(webp_size(&body), (256, 256));
    body
}

/// A preview that is not there: `404`, flagged with the state.
async fn no_preview(app: &TestApp, uri: &str, state: &str, message: &str) {
    let (status, headers, body) = fetch(&app.private, get_with(uri, &[])).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    assert_eq!(state_of(&headers), state);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "not_found");
    assert_eq!(body["error"]["message"], message, "{uri}");
}

#[tokio::test]
async fn image_gets_a_thumbnail_and_a_preview() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    // 600x300 with EXIF orientation 6: upright it is 300x600.
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("rotated.jpg")).await;
    let id = id_of(&blob);
    assert_eq!(blob["representations"]["thumbnail"]["state"], "pending");
    assert_eq!(blob["representations"]["preview"]["state"], "pending");
    assert_eq!(jobs_for(&app, id).await, ["render"]);
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    let preview = format!("/blobs/{id}/preview/photo.jpg");

    // Until the jobs run: the icon, not to be cached, and no preview.
    let placeholder = icon(&app, &thumbnail, "pending").await;
    no_preview(&app, &preview, "pending", "preview is pending").await;

    // One job renders both.
    assert!(run_job(&app, JobKind::Render).await);
    assert!(!run_job(&app, JobKind::Render).await);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["thumbnail"]["state"], "ready");
    assert_eq!(blob["representations"]["thumbnail"]["format"], "webp");
    assert_eq!(blob["representations"]["preview"]["state"], "ready");
    assert_eq!(blob["representations"]["preview"]["format"], "webp");
    assert_eq!(blob["representations"]["preview"]["source"], "image");

    // Fitted into the box, upright, never enlarged.
    let (headers, size) = rendered(&app, &thumbnail).await;
    assert_eq!(size, (128, 256));
    assert_eq!(
        headers["content-disposition"],
        "inline; filename=\"photo.webp\"; filename*=UTF-8''photo%2Ewebp"
    );
    assert_eq!(headers["x-content-type-options"], "nosniff");
    let etag = headers["etag"].to_str().unwrap().to_string();
    assert!(etag.starts_with('"') && etag.len() == 66, "{etag}");
    let (_, size) = rendered(&app, &preview).await;
    assert_eq!(size, (200, 400));

    // Conditional and HEAD requests.
    let (status, headers, body) = fetch(
        &app.private,
        get_with(&thumbnail, &[("if-none-match", &etag)]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers["etag"], etag.as_str());
    assert!(body.is_empty());
    let head = Request::builder()
        .method(Method::HEAD)
        .uri(&thumbnail)
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = fetch(&app.private, head).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["content-length"]
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap()
            > 0
    );
    assert!(body.is_empty());

    // The signed public URLs serve the same, and nothing else does.
    let (status, headers, body) =
        fetch(&app.public, get_with(&public_url(&blob, "thumbnail"), &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["etag"], etag.as_str());
    assert_eq!(webp_size(&body), (128, 256));
    assert_ne!(body, placeholder);
    let (status, _, body) = fetch(&app.public, get_with(&public_url(&blob, "preview"), &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(webp_size(&body), (200, 400));
    let (status, _) = call(
        &app.private,
        req(Method::GET, &format!("/blobs/{id}/thumbnail/other.jpg")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Stored under the blob's prefix, encrypted: not a WebP on disk.
    let mut keys: Vec<String> = stored_objects(&app)
        .await
        .into_iter()
        .map(|o| o.location.to_string())
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            object_key(id, "original"),
            object_key(id, "preview"),
            object_key(id, "thumbnail")
        ]
    );
    let stored = app
        .state
        .store()
        .get(&object_store::path::Path::from(object_key(id, "thumbnail")))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_ne!(&stored[..4], b"RIFF");
}

/// A JPEG XL upload is a picture like any other, rendered into the
/// deployment's format, WebP by default.
#[tokio::test]
async fn jxl_uploads_get_a_thumbnail_and_a_preview() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    // The rotated JPEG made upright by vips, 300x600.
    let blob = ready_file(&app, "photo.jxl", "image/jxl", &fixture("photo.jxl")).await;
    let id = id_of(&blob);
    assert_eq!(blob["category"], "image");
    render_all(&app).await;
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["preview"]["state"], "ready");
    assert_eq!(blob["representations"]["preview"]["source"], "image");
    assert_eq!(blob["representations"]["preview"]["format"], "webp");
    let (_, size) = rendered(&app, &format!("/blobs/{id}/thumbnail/photo.jxl")).await;
    assert_eq!(size, (128, 256));
    let (headers, size) = rendered(&app, &format!("/blobs/{id}/preview/photo.jxl")).await;
    assert_eq!(size, (200, 400));
    assert_eq!(
        headers["content-disposition"],
        "inline; filename=\"photo.webp\"; filename*=UTF-8''photo%2Ewebp"
    );
}

/// With `media.format = "jxl"`, the pictures are JPEG XL, named and typed
/// as such on the row, in the metadata, in the envelope and in the
/// response, and so is the icon served in a thumbnail's place.
#[tokio::test]
async fn jxl_is_an_opt_in_output_format() {
    use agrafe::blob::ImageFormat;

    let db = TestDatabase::new().await;
    let app = setup_tuned(db, fake_clamd().await, |c| {
        c.media.render.format = ImageFormat::Jxl;
    })
    .await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("rotated.jpg")).await;
    let id = id_of(&blob);
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    let preview = format!("/blobs/{id}/preview/photo.jpg");

    // The placeholder is the JPEG XL icon, and its download name says so.
    let headers = pending_jxl_icon(&app, &thumbnail).await;
    assert_eq!(
        headers["content-disposition"],
        "inline; filename=\"photo.jxl\"; filename*=UTF-8''photo%2Ejxl"
    );

    assert!(run_job(&app, JobKind::Render).await);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["thumbnail"]["format"], "jxl");
    assert_eq!(blob["representations"]["preview"]["format"], "jxl");

    // The picture, from the row.
    let (headers, size) = rendered_jxl(&app, &thumbnail).await;
    assert_eq!(size, (128, 256));
    assert_eq!(
        headers["content-disposition"],
        "inline; filename=\"photo.jxl\"; filename*=UTF-8''photo%2Ejxl"
    );
    let etag = headers["etag"].to_str().unwrap().to_string();
    let (_, size) = rendered_jxl(&app, &preview).await;
    assert_eq!(size, (200, 400));

    // `HEAD` answers from the row, with the format it records.
    let head = Request::builder()
        .method(Method::HEAD)
        .uri(&thumbnail)
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = fetch(&app.private, head).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/jxl");
    assert!(body.is_empty());

    // The sealed public URL serves from the envelope, which carries the
    // format too.
    let (status, headers, body) =
        fetch(&app.public, get_with(&public_url(&blob, "thumbnail"), &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/jxl");
    assert_eq!(headers["etag"], etag.as_str());
    assert_eq!(jxl_size(&body), (128, 256));

    // A thumbnail regenerated on its own is made from the stored preview,
    // a JPEG XL the loader allowlist lets through.
    sqlx::query("UPDATE representations SET version = 0 WHERE blob_id = $1 AND kind = 'thumbnail'")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    pending_jxl_icon(&app, &thumbnail).await;
    assert!(run_job(&app, JobKind::Render).await);
    let (_, size) = rendered_jxl(&app, &thumbnail).await;
    assert_eq!(size, (128, 256));
}

/// The icon served in a pending thumbnail's place on a deployment with
/// `media.format = "jxl"`: a JPEG XL like the pictures.
async fn pending_jxl_icon(app: &TestApp, uri: &str) -> HeaderMap {
    let (status, headers, body) = fetch(&app.private, get_with(uri, &[])).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    assert_eq!(headers["content-type"], "image/jxl");
    assert_eq!(state_of(&headers), "pending");
    assert_eq!(cache_of(&headers), "no-store");
    assert!(!headers.contains_key("etag"));
    assert_eq!(jxl_size(&body), (256, 256));
    headers
}

/// A rendered representation of a deployment on `media.format = "jxl"`:
/// `200`, JPEG XL, with its size.
async fn rendered_jxl(app: &TestApp, uri: &str) -> (HeaderMap, (u32, u32)) {
    let (status, headers, body) = fetch(&app.private, get_with(uri, &[])).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    assert_eq!(headers["content-type"], "image/jxl");
    assert_eq!(state_of(&headers), "ready");
    assert_eq!(cache_of(&headers), "private, max-age=86400");
    assert_eq!(headers["content-length"], body.len().to_string().as_str());
    (headers, jxl_size(&body))
}

/// The `Accept` header of a browser that decodes JPEG XL, and of one that
/// does not and offers `*/*` instead.
const ACCEPTS_JXL: &str =
    "image/avif,image/jxl,image/webp,image/png,image/svg+xml,image/*;q=0.8,*/*;q=0.5";
const ACCEPTS_ANY: &str = "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8";

/// The hex `ETag` of a picture the metadata lists with a base64 checksum.
fn etag_of(entry: &Value) -> String {
    use base64::Engine;
    let digest = base64::engine::general_purpose::STANDARD
        .decode(entry["checksum"].as_str().unwrap())
        .unwrap();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

/// With `media.webp_fallback`, a WebP is stored beside each JPEG XL
/// picture and served to the clients that do not say they decode JPEG
/// XL, or on request, with `Vary: Accept` when the client's header chose.
#[tokio::test]
async fn the_webp_fallback_stands_beside_a_jpeg_xl_picture() {
    use agrafe::blob::ImageFormat;

    let db = TestDatabase::new().await;
    let app = setup_tuned(db, fake_clamd().await, |c| {
        c.media.render.format = ImageFormat::Jxl;
        c.media.render.webp_fallback = true;
    })
    .await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("rotated.jpg")).await;
    let id = id_of(&blob);
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    let preview = format!("/blobs/{id}/preview/photo.jpg");

    // The icon follows the same rule: WebP for a client that does not
    // claim JPEG XL, JPEG XL for one that does, the one asked for by name.
    for (accept, format, size) in [
        (
            ACCEPTS_ANY,
            "image/webp",
            webp_size as fn(&[u8]) -> (u32, u32),
        ),
        (ACCEPTS_JXL, "image/jxl", jxl_size),
    ] {
        let (status, headers, body) =
            fetch(&app.private, get_with(&thumbnail, &[("accept", accept)])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], format);
        assert_eq!(headers["vary"], "accept");
        assert_eq!(state_of(&headers), "pending");
        assert_eq!(size(&body), (256, 256));
    }
    let (_, headers, body) = fetch(
        &app.private,
        get_with(&format!("{thumbnail}?format=jxl"), &[]),
    )
    .await;
    assert_eq!(headers["content-type"], "image/jxl");
    assert!(!headers.contains_key("vary"));
    assert_eq!(jxl_size(&body), (256, 256));

    assert!(run_job(&app, JobKind::Render).await);
    let blob = get_blob(&app, id).await;
    for kind in ["thumbnail", "preview"] {
        let entry = &blob["representations"][kind];
        assert_eq!(entry["format"], "jxl");
        assert_eq!(entry["fallback"]["format"], "webp", "{entry}");
        assert!(entry["fallback"]["checksum"].is_string());
        assert!(entry["fallback"]["size"].as_i64().unwrap() > 0);
        assert_ne!(entry["fallback"]["checksum"], entry["checksum"]);
    }
    // Both objects, the fallback under the one key with an extension.
    for name in ["thumbnail", "thumbnail.webp", "preview", "preview.webp"] {
        app.state
            .store()
            .head(&object_store::path::Path::from(object_key(id, name)))
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    let picture_etag = etag_of(&blob["representations"]["thumbnail"]);
    let picture_size = blob["representations"]["thumbnail"]["size"]
        .as_i64()
        .unwrap();
    let fallback_etag = etag_of(&blob["representations"]["thumbnail"]["fallback"]);
    let fallback_size = blob["representations"]["thumbnail"]["fallback"]["size"]
        .as_i64()
        .unwrap();

    // No Accept at all, which no browser sends: the picture as stored.
    let (status, headers, body) = fetch(&app.private, get_with(&thumbnail, &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/jxl");
    assert_eq!(headers["vary"], "accept");
    assert_eq!(headers["etag"], picture_etag.as_str());
    assert_eq!(headers["content-length"], picture_size.to_string().as_str());
    assert_eq!(jxl_size(&body), (128, 256));
    // A browser that offers */* and no more: the WebP, which it decodes.
    let (status, headers, body) = fetch(
        &app.private,
        get_with(&thumbnail, &[("accept", ACCEPTS_ANY)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/webp");
    assert_eq!(headers["vary"], "accept");
    assert_eq!(headers["etag"], fallback_etag.as_str());
    assert_eq!(
        headers["content-length"],
        fallback_size.to_string().as_str()
    );
    assert_eq!(
        headers["content-disposition"],
        "inline; filename=\"photo.webp\"; filename*=UTF-8''photo%2Ewebp"
    );
    assert_eq!(webp_size(&body), (128, 256), "the same picture");
    // One that names image/jxl: the picture.
    let (_, headers, body) =
        fetch(&app.private, get_with(&preview, &[("accept", ACCEPTS_JXL)])).await;
    assert_eq!(headers["content-type"], "image/jxl");
    assert_eq!(headers["vary"], "accept");
    assert_eq!(jxl_size(&body), (200, 400));

    // Asked for by name, Accept has no say and the response does not vary.
    let (_, headers, body) = fetch(
        &app.private,
        get_with(
            &format!("{thumbnail}?format=webp"),
            &[("accept", ACCEPTS_JXL)],
        ),
    )
    .await;
    assert_eq!(headers["content-type"], "image/webp");
    assert!(!headers.contains_key("vary"));
    assert_eq!(webp_size(&body), (128, 256));
    let (_, headers, body) = fetch(
        &app.private,
        get_with(
            &format!("{thumbnail}?format=jxl"),
            &[("accept", ACCEPTS_ANY)],
        ),
    )
    .await;
    assert_eq!(headers["content-type"], "image/jxl");
    assert_eq!(headers["etag"], picture_etag.as_str());
    assert!(!headers.contains_key("vary"));
    assert_eq!(jxl_size(&body), (128, 256));
    // A format that is not one: the minter's mistake.
    let (status, _, body) = fetch(
        &app.private,
        get_with(&format!("{thumbnail}?format=avif"), &[]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("webp or jxl"));

    // HEAD and 304 from the row, for the chosen one.
    let head = Request::builder()
        .method(Method::HEAD)
        .uri(&thumbnail)
        .header("accept", ACCEPTS_ANY)
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = fetch(&app.private, head).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/webp");
    assert_eq!(
        headers["content-length"],
        fallback_size.to_string().as_str()
    );
    assert!(body.is_empty());
    let (status, _, _) = fetch(
        &app.private,
        get_with(
            &thumbnail,
            &[("accept", ACCEPTS_ANY), ("if-none-match", &fallback_etag)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    let (status, _, _) = fetch(
        &app.private,
        get_with(
            &thumbnail,
            &[("accept", ACCEPTS_ANY), ("if-none-match", &picture_etag)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the other picture's tag");
    let (status, _, _) = fetch(
        &app.private,
        get_with(&thumbnail, &[("if-none-match", &picture_etag)]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);

    // The envelope carries the fallback, so the sealed path serves it too,
    // negotiated or by name.
    let sealed = public_url(&blob, "thumbnail");
    let (status, headers, body) =
        fetch(&app.public, get_with(&sealed, &[("accept", ACCEPTS_ANY)])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/webp");
    assert_eq!(headers["etag"], fallback_etag.as_str());
    assert_eq!(headers["vary"], "accept");
    assert_eq!(webp_size(&body), (128, 256));
    for accept in [Some(ACCEPTS_JXL), None] {
        let headers: Vec<(&str, &str)> = accept.map(|a| ("accept", a)).into_iter().collect();
        let (_, headers, _) = fetch(&app.public, get_with(&sealed, &headers)).await;
        assert_eq!(headers["content-type"], "image/jxl", "{accept:?}");
        assert_eq!(headers["etag"], picture_etag.as_str());
    }
    let (status, minted) = call(
        &app.private,
        json_req(
            Method::POST,
            &format!("/blobs/{id}/urls"),
            json!({"query": {"format": "webp"}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{minted}");
    let (_, headers, _) = fetch(
        &app.public,
        get_with(
            &public_path(minted["preview"].as_str().unwrap()),
            &[("accept", ACCEPTS_JXL)],
        ),
    )
    .await;
    assert_eq!(headers["content-type"], "image/webp");
    assert!(!headers.contains_key("vary"));
}

/// A JPEG XL picture without the WebP fallback the deployment stores,
/// rendered before `media.webp_fallback` was turned on, would go to the
/// browsers the fallback is for as it is: it is regenerated on its first
/// request, as a picture of another representation version is, so turning the
/// fallback on takes no bump. An envelope that records no fallback defers
/// to the row.
#[tokio::test]
async fn a_picture_without_the_fallback_the_deployment_stores_is_regenerated() {
    use agrafe::blob::ImageFormat;

    let db = TestDatabase::new().await;
    let app = setup_tuned(db, fake_clamd().await, |c| {
        c.media.render.format = ImageFormat::Jxl;
        c.media.render.webp_fallback = true;
    })
    .await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("rotated.jpg")).await;
    let id = id_of(&blob);
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    assert!(run_job(&app, JobKind::Render).await);

    // As a render before the setting left it: the picture alone, on the
    // row and in the bucket.
    let fallback = object_store::path::Path::from(object_key(id, "thumbnail.webp"));
    app.state.store().delete(&fallback).await.unwrap();
    sqlx::query(
        "UPDATE representations SET fallback_sha256 = NULL, fallback_size = NULL \
         WHERE blob_id = $1 AND kind = 'thumbnail'",
    )
    .bind(parse_id(id))
    .execute(app.state.pool())
    .await
    .unwrap();
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["thumbnail"]["state"], "ready");
    assert!(
        blob["representations"]["thumbnail"]
            .get("fallback")
            .is_none()
    );
    // Minted now, the envelope records no fallback either.
    let sealed = public_url(&blob, "thumbnail");

    // The first request gets the icon and queues the render, the sealed
    // path having handed the picture to the row.
    let (status, headers, body) =
        fetch(&app.public, get_with(&sealed, &[("accept", ACCEPTS_ANY)])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/webp");
    assert_eq!(state_of(&headers), "pending");
    assert_eq!(webp_size(&body), (256, 256), "the icon");
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["thumbnail"]["state"], "pending");
    assert!(run_job(&app, JobKind::Render).await);
    let blob = get_blob(&app, id).await;
    assert_eq!(
        blob["representations"]["thumbnail"]["fallback"]["format"],
        "webp"
    );
    let (status, headers, body) = fetch(
        &app.private,
        get_with(&thumbnail, &[("accept", ACCEPTS_ANY)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/webp");
    assert_eq!(state_of(&headers), "ready");
    assert_eq!(webp_size(&body), (128, 256));
    // The stale envelope still defers to the row, which now has both.
    let (_, headers, body) =
        fetch(&app.public, get_with(&sealed, &[("accept", ACCEPTS_ANY)])).await;
    assert_eq!(headers["content-type"], "image/webp");
    assert_eq!(state_of(&headers), "ready");
    assert_eq!(webp_size(&body), (128, 256));
}

/// A deployment without the fallback stores one picture: `format` gets it
/// whichever it names, nothing varies on `Accept`, and the minter is told
/// which formats it may ask for.
#[tokio::test]
async fn without_the_fallback_there_is_one_picture() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("rotated.jpg")).await;
    let id = id_of(&blob);
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    assert!(run_job(&app, JobKind::Render).await);
    let blob = get_blob(&app, id).await;
    assert!(
        blob["representations"]["thumbnail"]
            .get("fallback")
            .is_none()
    );

    let (_, headers, _) = fetch(
        &app.private,
        get_with(&thumbnail, &[("accept", ACCEPTS_JXL)]),
    )
    .await;
    assert_eq!(headers["content-type"], "image/webp");
    assert!(!headers.contains_key("vary"));
    let (status, headers, _) = fetch(
        &app.private,
        get_with(&format!("{thumbnail}?format=webp"), &[]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/webp");
    // A format the picture is not stored in: what is stored, as it is.
    let (status, headers, _) = fetch(
        &app.private,
        get_with(&format!("{thumbnail}?format=jxl"), &[]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/webp");
    assert!(!headers.contains_key("vary"));

    for (query, status, needle) in [
        (json!({"format": "webp"}), StatusCode::OK, ""),
        (
            json!({"format": "jxl"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "no picture is stored as jxl",
        ),
        (
            json!({"format": "avif"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "webp or jxl",
        ),
    ] {
        let (got, body) = call(
            &app.private,
            json_req(
                Method::POST,
                &format!("/blobs/{id}/urls"),
                json!({"query": query}),
            ),
        )
        .await;
        assert_eq!(got, status, "{query}: {body}");
        if !needle.is_empty() {
            assert!(
                body["error"]["message"].as_str().unwrap().contains(needle),
                "{body}"
            );
        }
    }
}

/// The fallback turned off and the version bumped: the render that
/// regenerates the picture removes the fallback the row still records,
/// and only then, so a deployment without the fallback never deletes.
#[tokio::test]
async fn a_fallback_no_longer_wanted_goes_with_the_regeneration() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("rotated.jpg")).await;
    let id = id_of(&blob);
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    assert!(run_job(&app, JobKind::Render).await);

    // What a JPEG XL deployment with the fallback left behind, as this
    // one's configuration finds it: a picture of an older version with its
    // fallback recorded on the row and in the bucket.
    let fallback = object_store::path::Path::from(object_key(id, "thumbnail.webp"));
    app.state
        .store()
        .put(&fallback, vec![0u8; 10].into())
        .await
        .unwrap();
    sqlx::query(
        "UPDATE representations SET version = 0, format = 'jxl', fallback_sha256 = $2, \
         fallback_size = 10 WHERE blob_id = $1 AND kind = 'thumbnail'",
    )
    .bind(parse_id(id))
    .bind(vec![1u8; 32])
    .execute(app.state.pool())
    .await
    .unwrap();
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["thumbnail"]["fallback"]["size"], 10);

    // The request notices the version, and the row keeps the fallback
    // through the pending render, which is not the client's business.
    icon(&app, &thumbnail, "pending").await;
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["thumbnail"]["state"], "pending");
    assert!(
        blob["representations"]["thumbnail"]
            .get("fallback")
            .is_none()
    );
    let row: (Option<Vec<u8>>, Option<i64>) = sqlx::query_as(
        "SELECT fallback_sha256, fallback_size FROM representations \
         WHERE blob_id = $1 AND kind = 'thumbnail'",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    assert_eq!(row, (Some(vec![1u8; 32]), Some(10)), "kept for the render");
    assert!(run_job(&app, JobKind::Render).await);
    let (headers, size) = rendered(&app, &thumbnail).await;
    assert_eq!(size, (128, 256));
    assert!(!headers.contains_key("vary"));
    let blob = get_blob(&app, id).await;
    assert!(
        blob["representations"]["thumbnail"]
            .get("fallback")
            .is_none()
    );
    let row: (Option<Vec<u8>>, Option<i64>) = sqlx::query_as(
        "SELECT fallback_sha256, fallback_size FROM representations \
         WHERE blob_id = $1 AND kind = 'thumbnail'",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    assert_eq!(row, (None, None));
    assert!(
        matches!(
            app.state.store().head(&fallback).await,
            Err(object_store::Error::NotFound { .. })
        ),
        "the fallback object is gone"
    );
}

#[tokio::test]
async fn vector_images_fill_the_box() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    // 20x10 nominal: a raster that small would stay small.
    let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="red"/></svg>"#;
    let blob = ready_file(&app, "logo.svg", "image/svg+xml", svg).await;
    let id = id_of(&blob);
    render_all(&app).await;
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["preview"]["source"], "image");
    let (_, size) = rendered(&app, &format!("/blobs/{id}/thumbnail/logo.svg")).await;
    assert_eq!(size, (256, 128));
    let (_, size) = rendered(&app, &format!("/blobs/{id}/preview/logo.svg")).await;
    assert_eq!(size, (400, 200));
}

#[tokio::test]
async fn pdf_previews_come_from_the_embedded_image_only() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    // One JPEG covering a page a tenth of its size: the image itself is the
    // preview, at its own 600x300 fitted into 400.
    let wrapped = ready_file(&app, "scan.pdf", "application/pdf", &fixture("image.pdf")).await;
    let id = id_of(&wrapped);
    // The same image with a paragraph beside it, and a page of text: real
    // documents, with no preview, and the first page as their thumbnail.
    // Being vector, the page fills the box whatever its nominal size.
    let mixed = ready_file(&app, "mixed.pdf", "application/pdf", &fixture("mixed.pdf")).await;
    let text = ready_file(&app, "text.pdf", "application/pdf", &fixture("text.pdf")).await;
    render_all(&app).await;

    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["preview"]["state"], "ready");
    assert_eq!(
        blob["representations"]["preview"]["source"],
        "embedded_image"
    );
    assert_eq!(blob["representations"]["thumbnail"]["state"], "ready");
    let (_, size) = rendered(&app, &format!("/blobs/{id}/preview/scan.pdf")).await;
    assert_eq!(size, (400, 200));
    let (_, size) = rendered(&app, &format!("/blobs/{id}/thumbnail/scan.pdf")).await;
    assert_eq!(size, (256, 128));

    for blob in [&mixed, &text] {
        let id = id_of(blob);
        let name = blob["filename"].as_str().unwrap();
        let blob = get_blob(&app, id).await;
        assert_eq!(
            blob["representations"]["preview"]["state"], "unavailable",
            "{name}"
        );
        assert_eq!(
            blob["representations"]["preview"]["source"],
            Value::Null,
            "{name}"
        );
        assert_eq!(
            blob["representations"]["thumbnail"]["state"], "ready",
            "{name}"
        );
        no_preview(
            &app,
            &format!("/blobs/{id}/preview/{name}"),
            "unavailable",
            "no preview for this file",
        )
        .await;
        let (_, size) = rendered(&app, &format!("/blobs/{id}/thumbnail/{name}")).await;
        assert_eq!(size, (256, 128), "{name}");
    }
}

#[tokio::test]
async fn office_documents_wrapping_a_picture() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let docx = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
    let odt = "application/vnd.oasis.opendocument.text";
    let picture = ready_file(&app, "picture.docx", docx, &fixture("image.docx")).await;
    let letter = ready_file(&app, "letter.docx", docx, &fixture("text.docx")).await;
    let odt_picture = ready_file(&app, "picture.odt", odt, &fixture("image.odt")).await;
    // A macro-enabled variant is the same package, opened the same way.
    let docm_picture = ready_file(&app, "picture.docm", DOCM, &fixture("image.docx")).await;
    render_all(&app).await;

    for blob in [&picture, &odt_picture, &docm_picture] {
        let id = id_of(blob);
        let name = blob["filename"].as_str().unwrap();
        let blob = get_blob(&app, id).await;
        assert_eq!(
            blob["representations"]["preview"]["source"], "embedded_image",
            "{name}"
        );
        let (_, size) = rendered(&app, &format!("/blobs/{id}/preview/{name}")).await;
        assert_eq!(size, (64, 32), "{name}");
        let (_, size) = rendered(&app, &format!("/blobs/{id}/thumbnail/{name}")).await;
        assert_eq!(size, (64, 32), "{name}");
    }

    // A real document with no converter configured: no preview, and the
    // icon stays.
    let id = id_of(&letter);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["preview"]["state"], "unavailable");
    assert_eq!(blob["representations"]["preview"]["source"], Value::Null);
    assert_eq!(blob["representations"]["thumbnail"]["state"], "unavailable");
    let docx_icon = icon(
        &app,
        &format!("/blobs/{id}/thumbnail/letter.docx"),
        "unavailable",
    )
    .await;
    no_preview(
        &app,
        &format!("/blobs/{id}/preview/letter.docx"),
        "unavailable",
        "no preview for this file",
    )
    .await;
    // The icon carries the type badge, so it differs from a generic one.
    let blob = ready_file(&app, "blob.bin", "application/octet-stream", b"whatever").await;
    let id = id_of(&blob);
    render_all(&app).await;
    let generic_icon = icon(
        &app,
        &format!("/blobs/{id}/thumbnail/blob.bin"),
        "unavailable",
    )
    .await;
    assert_ne!(docx_icon, generic_icon);
}

const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const DOCM: &str = "application/vnd.ms-word.document.macroenabled.12";
const ODT: &str = "application/vnd.oasis.opendocument.text";
const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const XLSM: &str = "application/vnd.ms-excel.sheet.macroenabled.12";
const ODS: &str = "application/vnd.oasis.opendocument.spreadsheet";
const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
const ODP: &str = "application/vnd.oasis.opendocument.presentation";
const ODG: &str = "application/vnd.oasis.opendocument.graphics";

/// LibreOffice on the `PATH`, or `None` with a message: the converter
/// tests run where it is installed, which CI is.
fn soffice() -> Option<&'static str> {
    let found = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("soffice").is_file()));
    if !found {
        eprintln!("skipped: no soffice on PATH");
    }
    found.then_some("soffice")
}

#[tokio::test]
async fn office_documents_get_their_first_page_through_libreoffice() {
    let db = TestDatabase::new().await;
    let Some(soffice) = soffice() else {
        return;
    };
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.media.render.office.command = Some(soffice.to_string())
    })
    .await;
    // A two-page report, a letter and a table in each format: every one
    // gets its first page, an A4 or Letter portrait page filling the box's
    // height, and none a preview.
    let documents = [
        ready_file(&app, "report.docx", DOCX, &fixture("pages.docx")).await,
        ready_file(&app, "letter.odt", ODT, &fixture("text.odt")).await,
        ready_file(&app, "table.xlsx", XLSX, &fixture("table.xlsx")).await,
        ready_file(&app, "table.ods", ODS, &fixture("table.ods")).await,
    ];
    // A DOCX wrapping a picture still takes the embedded-image path, and
    // one whose picture is beside a paragraph is a real document.
    let picture = ready_file(&app, "picture.docx", DOCX, &fixture("image.docx")).await;
    let mixed = ready_file(&app, "mixed.docx", DOCX, &fixture("text.docx")).await;
    // A well-formed ZIP whose document part is cut mid-element: LibreOffice
    // cannot load it and exits non-zero, so the file is at fault.
    let broken = ready_file(&app, "broken.docx", DOCX, &fixture("broken.docx")).await;
    render_all(&app).await;

    for blob in documents.iter().chain([&mixed]) {
        let id = id_of(blob);
        let name = blob["filename"].as_str().unwrap();
        let blob = get_blob(&app, id).await;
        assert_eq!(
            blob["representations"]["preview"]["state"], "unavailable",
            "{name}"
        );
        assert_eq!(
            blob["representations"]["thumbnail"]["state"], "ready",
            "{name}"
        );
        no_preview(
            &app,
            &format!("/blobs/{id}/preview/{name}"),
            "unavailable",
            "no preview for this file",
        )
        .await;
        let (_, (width, height)) = rendered(&app, &format!("/blobs/{id}/thumbnail/{name}")).await;
        assert_eq!(height, 256, "{name}");
        assert!((150..=200).contains(&width), "{name}: {width}");
    }
    let blob = get_blob(&app, id_of(&picture)).await;
    assert_eq!(
        blob["representations"]["preview"]["source"],
        "embedded_image"
    );
    assert_eq!(blob["representations"]["thumbnail"]["state"], "ready");
    let id = id_of(&broken);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["thumbnail"]["state"], "failed");
    icon(
        &app,
        &format!("/blobs/{id}/thumbnail/broken.docx"),
        "failed",
    )
    .await;
}

#[tokio::test]
async fn the_startup_probe_checks_libreoffice() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let media = &app.state.config().media;
    let probe = |render| agrafe_render::probe(render, media.tmp_dir.as_deref());
    let with = |command: &str| agrafe_render::Config {
        office: agrafe_render::Office {
            command: Some(command.to_string()),
            ..Default::default()
        },
        ..media.render.clone()
    };

    // A binary that exits non-zero on everything, and one that exits zero
    // without writing anything.
    let wrong = with("/usr/bin/false");
    let err = probe(&wrong).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("`/usr/bin/false` failed on a two-page DOCX"),
        "{err}"
    );
    assert!(
        err.to_string()
            .contains("is media.office.command a LibreOffice 7.4 or later"),
        "{err}"
    );
    let idle = with("/usr/bin/true");
    let err = probe(&idle).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("`/usr/bin/true` wrote no PDF for a two-page DOCX"),
        "{err}"
    );
    let missing = with("/nonexistent/soffice");
    let err = probe(&missing).await.unwrap_err();
    assert!(
        err.to_string().contains("starting `/nonexistent/soffice`"),
        "{err}"
    );

    if let Some(soffice) = soffice() {
        let real = with(soffice);
        probe(&real).await.unwrap();
    }
}

/// The legacy binary formats and the variants go through the same
/// converter under their own extension: a Word 97 letter and an RTF one
/// through Writer, an Excel 97 table and a macro-enabled workbook through
/// Calc, each a portrait page, and each classified with its family.
#[tokio::test]
async fn legacy_and_variant_office_files_get_their_first_page_too() {
    let db = TestDatabase::new().await;
    let Some(soffice) = soffice() else {
        return;
    };
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.media.render.office.command = Some(soffice.to_string())
    })
    .await;
    let documents = [
        (
            ready_file(
                &app,
                "letter.doc",
                "application/msword",
                &fixture("letter.doc"),
            )
            .await,
            "document",
        ),
        (
            ready_file(&app, "letter.rtf", "text/rtf", &fixture("letter.rtf")).await,
            "document",
        ),
        (
            ready_file(
                &app,
                "table.xls",
                "application/vnd.ms-excel",
                &fixture("table.xls"),
            )
            .await,
            "spreadsheet",
        ),
        (
            ready_file(&app, "table.xlsm", XLSM, &fixture("table.xlsm")).await,
            "spreadsheet",
        ),
    ];
    render_all(&app).await;

    for (blob, category) in &documents {
        let id = id_of(blob);
        let name = blob["filename"].as_str().unwrap();
        let blob = get_blob(&app, id).await;
        assert_eq!(blob["category"], *category, "{name}");
        assert_eq!(
            blob["representations"]["preview"]["state"], "unavailable",
            "{name}"
        );
        assert_eq!(
            blob["representations"]["thumbnail"]["state"], "ready",
            "{name}"
        );
        no_preview(
            &app,
            &format!("/blobs/{id}/preview/{name}"),
            "unavailable",
            "no preview for this file",
        )
        .await;
        let (_, (width, height)) = rendered(&app, &format!("/blobs/{id}/thumbnail/{name}")).await;
        assert_eq!(height, 256, "{name}");
        assert!((150..=200).contains(&width), "{name}: {width}");
    }
}

/// Presentations through Impress and a drawing through Draw: a PPTX, an
/// ODP and a PowerPoint 97 file each get their first slide, an ODG its
/// first page, all 4:3 landscape, with no preview; a broken PPTX fails
/// like a broken DOCX.
#[tokio::test]
async fn presentations_and_drawings_get_their_first_slide_through_libreoffice() {
    let db = TestDatabase::new().await;
    let Some(soffice) = soffice() else {
        return;
    };
    let clamd = fake_clamd().await;
    // The PowerPoint 97 file LibreOffice writes is about 600 KB.
    let app = setup_tuned(db, clamd, |c| {
        c.media.render.office.command = Some(soffice.to_string());
        c.upload.max_size = bytesize::ByteSize::mib(1);
    })
    .await;
    let slides = [
        (
            ready_file(&app, "slides.pptx", PPTX, &fixture("slides.pptx")).await,
            "presentation",
        ),
        (
            ready_file(&app, "slides.odp", ODP, &fixture("slides.odp")).await,
            "presentation",
        ),
        (
            ready_file(
                &app,
                "slides.ppt",
                "application/vnd.ms-powerpoint",
                &fixture("slides.ppt"),
            )
            .await,
            "presentation",
        ),
        (
            ready_file(&app, "drawing.odg", ODG, &fixture("drawing.odg")).await,
            "document",
        ),
    ];
    let broken = ready_file(&app, "broken.pptx", PPTX, &fixture("broken.pptx")).await;
    render_all(&app).await;

    for (blob, category) in &slides {
        let id = id_of(blob);
        let name = blob["filename"].as_str().unwrap();
        let blob = get_blob(&app, id).await;
        assert_eq!(blob["category"], *category, "{name}");
        assert_eq!(
            blob["representations"]["preview"]["state"], "unavailable",
            "{name}"
        );
        assert_eq!(
            blob["representations"]["thumbnail"]["state"], "ready",
            "{name}"
        );
        no_preview(
            &app,
            &format!("/blobs/{id}/preview/{name}"),
            "unavailable",
            "no preview for this file",
        )
        .await;
        let (_, (width, height)) = rendered(&app, &format!("/blobs/{id}/thumbnail/{name}")).await;
        assert_eq!(width, 256, "{name}");
        assert!((190..=194).contains(&height), "{name}: {height}");
    }
    let id = id_of(&broken);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["thumbnail"]["state"], "failed");
    icon(
        &app,
        &format!("/blobs/{id}/thumbnail/broken.pptx"),
        "failed",
    )
    .await;
}

#[tokio::test]
async fn text_files_get_a_thumbnail_and_no_preview() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    // Latin-1, as administrative CSVs tend to be.
    let csv = b"nom;ville;montant\nDupont;Besan\xe7on;12,50\nMartin;Orl\xe9ans;7,00\n";
    let table = ready_file(&app, "export.csv", "text/csv", csv).await;
    let notes = ready_file(
        &app,
        "notes.txt",
        "text/plain",
        "Bonjour à tous\n".as_bytes(),
    )
    .await;
    let config = ready_file(&app, "config.json", "application/json", b"{\"a\": 1}\n").await;
    // Declared as text, but not text.
    let binary: Vec<u8> = (0u8..=255).cycle().take(2000).collect();
    let fake = ready_file(&app, "fake.txt", "text/plain", &binary).await;
    render_all(&app).await;

    for blob in [&table, &notes, &config] {
        let id = id_of(blob);
        let name = blob["filename"].as_str().unwrap();
        let blob = get_blob(&app, id).await;
        assert_eq!(
            blob["representations"]["thumbnail"]["state"], "ready",
            "{name}"
        );
        assert_eq!(
            blob["representations"]["preview"]["state"], "unavailable",
            "{name}"
        );
        assert_eq!(
            blob["representations"]["preview"]["source"],
            Value::Null,
            "{name}"
        );
        let (_, size) = rendered(&app, &format!("/blobs/{id}/thumbnail/{name}")).await;
        assert_eq!(size, (256, 256), "{name}");
        no_preview(
            &app,
            &format!("/blobs/{id}/preview/{name}"),
            "unavailable",
            "no preview for this file",
        )
        .await;
    }
    // Two different files give two different pages.
    let page_of = |blob: &Value| {
        format!(
            "/blobs/{}/thumbnail/{}",
            id_of(blob),
            blob["filename"].as_str().unwrap()
        )
    };
    let (_, _, a) = fetch(&app.private, get_with(&page_of(&table), &[])).await;
    let (_, _, b) = fetch(&app.private, get_with(&page_of(&notes), &[])).await;
    assert_ne!(a, b);

    let id = id_of(&fake);
    assert_eq!(
        get_blob(&app, id).await["representations"]["thumbnail"]["state"],
        "unavailable"
    );
    icon(
        &app,
        &format!("/blobs/{id}/thumbnail/fake.txt"),
        "unavailable",
    )
    .await;
}

#[tokio::test]
async fn a_crashing_helper_fails_the_file_not_the_job() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.media.render.extract_command = "/usr/bin/false".into();
        c.media.render.render_text_command = "/usr/bin/false".into();
    })
    .await;
    let notes = ready_file(&app, "notes.txt", "text/plain", b"plain text\n").await;
    let letter = ready_file(
        &app,
        "picture.docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        &fixture("image.docx"),
    )
    .await;
    render_all(&app).await;
    for (blob, name) in [(&notes, "notes.txt"), (&letter, "picture.docx")] {
        let id = id_of(blob);
        let blob = get_blob(&app, id).await;
        assert_eq!(
            blob["representations"]["thumbnail"]["state"], "failed",
            "{name}"
        );
        assert_eq!(jobs_for(&app, id).await, Vec::<String>::new(), "{name}");
    }
    // A PDF has no other preview source, so the preview is failed; the
    // thumbnail falls back to the first page, which vips renders without
    // the helper.
    let scan = ready_file(&app, "scan.pdf", "application/pdf", &fixture("image.pdf")).await;
    // The same PDF with only a thumbnail asked for follows the same rule
    // from its own attempt at the picture.
    let mut thumbnail_only = blob_for(&fixture("image.pdf"), "application/pdf", "raw");
    thumbnail_only["filename"] = json!("thumb.pdf");
    thumbnail_only["skip_virus_scan"] = json!(true);
    thumbnail_only["representations"] = json!(["thumbnail"]);
    let thumbnail_only = create(&app, thumbnail_only).await;
    let (status, body) = put_private(&app, id_of(&thumbnail_only), fixture("image.pdf")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    render_all(&app).await;
    // A disabled preview has no entry at all.
    for (blob, name, preview) in [
        (&scan, "scan.pdf", json!("failed")),
        (&thumbnail_only, "thumb.pdf", Value::Null),
    ] {
        let id = id_of(blob);
        let blob = get_blob(&app, id).await;
        assert_eq!(
            blob["representations"]["preview"]["state"], preview,
            "{name}"
        );
        assert_eq!(
            blob["representations"]["thumbnail"]["state"], "ready",
            "{name}"
        );
        rendered(&app, &format!("/blobs/{id}/thumbnail/{name}")).await;
    }
}

/// A file that exhausts a tool's time limit is not handed to the next one
/// for as long again: the PDF's first page is not tried once the
/// extractor timed out on it, and both representations are failed.
#[tokio::test]
async fn a_timed_out_helper_fails_both_representations() {
    let db = TestDatabase::new().await;
    use std::os::unix::fs::PermissionsExt;
    let slow = std::env::temp_dir().join(format!("agrafe-slow-extract-{}", uuid::Uuid::now_v7()));
    std::fs::write(&slow, "#!/bin/sh\nsleep 5\n").unwrap();
    std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.media.render.extract_command = slow.to_string_lossy().into_owned();
        c.media.render.sandbox.timeout = std::time::Duration::from_millis(300);
    })
    .await;
    let scan = ready_file(&app, "scan.pdf", "application/pdf", &fixture("image.pdf")).await;
    render_all(&app).await;
    let id = id_of(&scan);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["preview"]["state"], "failed");
    assert_eq!(blob["representations"]["thumbnail"]["state"], "failed");
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());
    std::fs::remove_file(&slow).unwrap();
}

/// A later job renders the thumbnail over a preview that timed out: `failed`
/// as when one job did both, not the same bytes handed to the page renderer
/// for as long again.
#[tokio::test]
async fn a_thumbnail_over_a_stored_timeout_is_failed() {
    let db = TestDatabase::new().await;
    use std::os::unix::fs::PermissionsExt;
    let slow = std::env::temp_dir().join(format!("agrafe-slow-extract-{}", uuid::Uuid::now_v7()));
    std::fs::write(&slow, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    let clamd = fake_clamd().await;
    // Long enough for the page renderer the thumbnail must not reach.
    let app = setup_tuned(db, clamd, |c| {
        c.media.render.extract_command = slow.to_string_lossy().into_owned();
        c.media.render.sandbox.timeout = Duration::from_secs(2);
    })
    .await;
    let scan = ready_file(&app, "scan.pdf", "application/pdf", &fixture("image.pdf")).await;
    let id = id_of(&scan);
    render_all(&app).await;
    let flag = || {
        sqlx::query_scalar::<_, Option<bool>>(
            "SELECT timed_out FROM representations WHERE blob_id = $1 AND kind = 'preview'",
        )
        .bind(parse_id(id))
        .fetch_one(app.state.pool())
    };
    assert_eq!(flag().await.unwrap(), Some(true));

    // The thumbnail alone is stale: its job reads the preview's verdict.
    sqlx::query("UPDATE representations SET version = 0 WHERE blob_id = $1 AND kind = 'thumbnail'")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    icon(&app, &format!("/blobs/{id}/thumbnail/scan.pdf"), "pending").await;
    assert!(run_job(&app, JobKind::Render).await);
    let shown = get_blob(&app, id).await;
    assert_eq!(
        shown["representations"]["preview"]["state"], "failed",
        "{shown}"
    );
    assert_eq!(
        shown["representations"]["thumbnail"]["state"], "failed",
        "{shown}"
    );

    // A render that gets further clears it.
    sqlx::query("UPDATE representations SET version = 0 WHERE blob_id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    std::fs::write(&slow, "#!/bin/sh\nexit 0\n").unwrap();
    no_preview(
        &app,
        &format!("/blobs/{id}/preview/scan.pdf"),
        "pending",
        "preview is pending",
    )
    .await;
    assert!(run_job(&app, JobKind::Render).await);
    assert_eq!(flag().await.unwrap(), None);
    std::fs::remove_file(&slow).unwrap();
}

/// A row or an original that cannot be read is dead at its first attempt,
/// with its verdict on the blob, not three hours of retries behind a
/// `pending` thumbnail. A KEK this process lacks says nothing of the bytes
/// and is retried: a deploy brings it.
#[tokio::test]
async fn an_original_that_does_not_open_kills_the_render_at_once() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let job = |id: uuid::Uuid| {
        let pool = app.state.pool().clone();
        async move {
            sqlx::query_as::<_, (i32, bool)>(
                "SELECT attempts, failed_at IS NOT NULL FROM jobs \
                 WHERE blob_id = $1 AND kind = 'render'",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    // The row lacks a key, and the object does not authenticate.
    let no_key = ready_file(&app, "a.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let no_key = id_of(&no_key).to_string();
    sqlx::query("UPDATE blobs SET wrapped_key = NULL WHERE id = $1")
        .bind(parse_id(&no_key))
        .execute(app.state.pool())
        .await
        .unwrap();
    assert!(run_job(&app, JobKind::Render).await);
    assert_eq!(job(parse_id(&no_key)).await, (1, true), "no wrapped key");
    let shown = get_blob(&app, &no_key).await;
    assert!(shown["failed_at"].is_string(), "{shown}");

    let tampered = ready_file(&app, "b.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let tampered = id_of(&tampered).to_string();
    let key = object_store::path::Path::from(storage_key(&app, &tampered).await);
    let object = app.state.store().get(&key).await.unwrap();
    let mut bytes = object.bytes().await.unwrap().to_vec();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    app.state.store().put(&key, bytes.into()).await.unwrap();
    assert!(run_job(&app, JobKind::Render).await);
    assert_eq!(job(parse_id(&tampered)).await, (1, true), "tampered");

    let no_kek = ready_file(&app, "c.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let no_kek = id_of(&no_kek).to_string();
    sqlx::query("UPDATE blobs SET kek_id = 'gone' WHERE id = $1")
        .bind(parse_id(&no_kek))
        .execute(app.state.pool())
        .await
        .unwrap();
    assert!(run_job(&app, JobKind::Render).await);
    assert_eq!(job(parse_id(&no_kek)).await, (1, false), "unknown KEK");
    let shown = get_blob(&app, &no_kek).await;
    assert_eq!(shown["failed_at"], Value::Null, "{shown}");
}

#[tokio::test]
async fn files_the_renderer_rejects_are_failed() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "broken.jpg", "image/jpeg", &fixture("truncated.jpg")).await;
    let id = id_of(&blob);
    render_all(&app).await;
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["thumbnail"]["state"], "failed");
    assert_eq!(blob["representations"]["preview"]["state"], "failed");
    assert_eq!(
        jobs_for(&app, id).await,
        Vec::<String>::new(),
        "done, not retried"
    );
    icon(&app, &format!("/blobs/{id}/thumbnail/broken.jpg"), "failed").await;
    no_preview(
        &app,
        &format!("/blobs/{id}/preview/broken.jpg"),
        "failed",
        "preview could not be generated",
    )
    .await;

    // Declared PNG, actually XPM: the upload sniffer knows no XPM and lets
    // it through, and vips would hand it to ImageMagick. It never gets it.
    let xpm =
        b"/* XPM */\nstatic char *x[] = {\n\"2 2 1 1\",\n\"a c #ff0000\",\n\"aa\",\n\"aa\"\n};\n";
    let blob = ready_file(&app, "pic.png", "image/png", xpm).await;
    let id = id_of(&blob);
    render_all(&app).await;
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["thumbnail"]["state"], "failed");
    assert_eq!(blob["representations"]["preview"]["state"], "failed");
}

#[tokio::test]
async fn oversized_originals_get_no_representations() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.media.max_source_size = bytesize::ByteSize::b(1000)
    })
    .await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    // A text file over the limit still gets its thumbnail: the sample is
    // the first few KiB, however long the file.
    let long: Vec<u8> = b"line\n".repeat(400);
    let text = ready_file(&app, "long.txt", "text/plain", &long).await;
    // A PDF over the limit does not: its first page would decode the file.
    let pdf = ready_file(&app, "text.pdf", "application/pdf", &fixture("mixed.pdf")).await;
    render_all(&app).await;
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["thumbnail"]["state"], "unavailable");
    assert_eq!(blob["representations"]["preview"]["state"], "unavailable");
    let text = get_blob(&app, id_of(&text)).await;
    assert_eq!(text["representations"]["thumbnail"]["state"], "ready");
    assert_eq!(text["representations"]["preview"]["state"], "unavailable");
    let pdf = get_blob(&app, id_of(&pdf)).await;
    assert_eq!(pdf["representations"]["thumbnail"]["state"], "unavailable");
    assert_eq!(pdf["representations"]["preview"]["state"], "unavailable");
}

#[tokio::test]
async fn a_missing_renderer_fails_the_job_not_the_file() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.media.render.vips_command = "/nonexistent/vips".into()
    })
    .await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    assert!(run_job(&app, JobKind::Render).await);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["thumbnail"]["state"], "pending");
    assert_eq!(blob["representations"]["preview"]["state"], "pending");
    let error: String =
        sqlx::query_scalar("SELECT last_error FROM jobs WHERE blob_id = $1 AND kind = 'render'")
            .bind(parse_id(id))
            .fetch_one(app.state.pool())
            .await
            .unwrap();
    assert!(error.contains("starting `/nonexistent/vips`"), "{error}");
}

#[tokio::test]
async fn the_startup_probe_checks_vips_and_the_helper() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let media = &app.state.config().media;
    let probe = |render| agrafe_render::probe(render, media.tmp_dir.as_deref());
    probe(&media.render).await.unwrap();

    let no_vips = agrafe_render::Config {
        vips_command: "/nonexistent/vips".into(),
        ..media.render.clone()
    };
    let err = probe(&no_vips).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("rendering a PNG with `/nonexistent/vips`: starting"),
        "{err}"
    );

    // A binary that exits non-zero on everything, as a wrong program on the
    // path would.
    let wrong_binary = agrafe_render::Config {
        render_text_command: "/usr/bin/false".into(),
        ..media.render.clone()
    };
    let err = probe(&wrong_binary).await.unwrap_err();
    assert!(err.to_string().contains("failed on a text sample"), "{err}");

    // One that exits zero without doing anything.
    let idle_binary = agrafe_render::Config {
        render_text_command: "/usr/bin/true".into(),
        ..media.render.clone()
    };
    let err = probe(&idle_binary).await.unwrap_err();
    assert!(err.to_string().contains("declined a text sample"), "{err}");

    // The map helper is probed the same way, without a background layer.
    let wrong_geo = agrafe_render::Config {
        render_geo_command: "/usr/bin/false".into(),
        ..media.render.clone()
    };
    let err = probe(&wrong_geo).await.unwrap_err();
    assert!(
        err.to_string().contains("failed on a one-point GeoJSON"),
        "{err}"
    );
    assert!(
        err.to_string()
            .contains("is media.render_geo_command the agrafe-render-geo helper?"),
        "{err}"
    );
    let idle_geo = agrafe_render::Config {
        render_geo_command: "/usr/bin/true".into(),
        ..media.render.clone()
    };
    let err = probe(&idle_geo).await.unwrap_err();
    assert!(
        err.to_string().contains("declined a one-point GeoJSON"),
        "{err}"
    );

    // And the extractor, on a PDF wrapping a JPEG: one that is missing
    // fails every render job, one that declines every preview of a scan.
    let wrong_extract = agrafe_render::Config {
        extract_command: "/usr/bin/false".into(),
        ..media.render.clone()
    };
    let err = probe(&wrong_extract).await.unwrap_err();
    assert!(
        err.to_string().contains("failed on a PDF wrapping a JPEG"),
        "{err}"
    );
    assert!(
        err.to_string()
            .contains("is media.extract_command the agrafe-extract helper?"),
        "{err}"
    );
    let idle_extract = agrafe_render::Config {
        extract_command: "/usr/bin/true".into(),
        ..media.render.clone()
    };
    let err = probe(&idle_extract).await.unwrap_err();
    assert!(
        err.to_string().contains("declined a PDF wrapping a JPEG"),
        "{err}"
    );
    let missing_extract = agrafe_render::Config {
        extract_command: "/nonexistent/agrafe-extract".into(),
        ..media.render.clone()
    };
    let err = probe(&missing_extract).await.unwrap_err();
    assert!(
        err.to_string().contains("/nonexistent/agrafe-extract"),
        "{err}"
    );
}

#[tokio::test]
async fn blob_state_governs_the_representations() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;

    // No bytes yet: pending, with nothing to render from.
    let created = create(&app, raw_blob()).await;
    let id = id_of(&created);
    let thumbnail = format!("/blobs/{id}/thumbnail/photo%20%C3%A9t%C3%A9.jpg");
    let preview = format!("/blobs/{id}/preview/photo%20%C3%A9t%C3%A9.jpg");
    icon(&app, &thumbnail, "pending").await;
    let (status, body) = call(&app.private, req(Method::GET, &preview)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["message"], "blob is created and not readable");

    // Waiting for a deferred scan: still pending, and no job yet.
    let bytes = png_bytes(5000);
    let uploaded = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&uploaded);
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    icon(&app, &format!("/blobs/{id}/thumbnail/photo.png"), "pending").await;
    let (status, body) = call(
        &app.private,
        req(Method::GET, &format!("/blobs/{id}/preview/photo.png")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Infected: never. The public port does not even see the blob.
    let bytes = infected_png(1500);
    let infected = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&infected);
    let (status, _) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    icon(
        &app,
        &format!("/blobs/{id}/thumbnail/photo.png"),
        "unavailable",
    )
    .await;
    let (status, _) = call(
        &app.private,
        req(Method::GET, &format!("/blobs/{id}/preview/photo.png")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    for kind in ["thumbnail", "preview"] {
        let (status, _) = call(&app.public, req(Method::GET, &public_url(&infected, kind))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{kind}");
    }

    // Deleted after rendering: the private port answers as for infected,
    // and no regeneration is queued for a blob on its way out, only the
    // purge the delete scheduled.
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    render_all(&app).await;
    assert_eq!(delete(&app, id).await, StatusCode::NO_CONTENT);
    icon(
        &app,
        &format!("/blobs/{id}/thumbnail/photo.jpg"),
        "unavailable",
    )
    .await;
    assert_eq!(jobs_for(&app, id).await, vec!["purge"]);
}

#[tokio::test]
async fn conditional_and_head_requests_are_answered_from_the_row() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    render_all(&app).await;
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    let (headers, _) = rendered(&app, &thumbnail).await;
    let etag = headers["etag"].to_str().unwrap().to_string();
    let length = headers["content-length"].clone();

    // The ETag is the digest the row records: the one the bytes have.
    let (digest, size): (Vec<u8>, i64) = sqlx::query_as(
        "SELECT sha256, size FROM representations WHERE blob_id = $1 AND kind = 'thumbnail'",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(etag, format!("\"{hex}\""));
    assert_eq!(length.to_str().unwrap(), size.to_string());

    // With the object gone, a conditional request and a HEAD still answer
    // from the row, and queue nothing: the object was never read. The GET
    // that needs the bytes is the one that finds it missing.
    app.state
        .store()
        .delete(&object_store::path::Path::from(object_key(id, "thumbnail")))
        .await
        .unwrap();
    let (status, headers, body) = fetch(
        &app.private,
        get_with(&thumbnail, &[("if-none-match", &etag)]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers["etag"], etag.as_str());
    assert!(body.is_empty());
    let head = Request::builder()
        .method(Method::HEAD)
        .uri(&thumbnail)
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = fetch(&app.private, head).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["etag"], etag.as_str());
    assert_eq!(headers["content-length"], length);
    assert!(body.is_empty());
    assert!(
        jobs_for(&app, id).await.is_empty(),
        "nothing read the object"
    );
    icon(&app, &thumbnail, "pending").await;
    assert_eq!(jobs_for(&app, id).await, ["render"]);
}

#[tokio::test]
async fn representations_regenerate_when_missing_or_stale() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    render_all(&app).await;
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    let preview = format!("/blobs/{id}/preview/photo.jpg");

    // The object vanished from the store: back to pending, queued again.
    app.state
        .store()
        .delete(&object_store::path::Path::from(object_key(id, "thumbnail")))
        .await
        .unwrap();
    icon(&app, &thumbnail, "pending").await;
    assert_eq!(
        get_blob(&app, id).await["representations"]["thumbnail"]["state"],
        "pending"
    );
    assert_eq!(jobs_for(&app, id).await, ["render"]);
    icon(&app, &thumbnail, "pending").await;
    assert_eq!(jobs_for(&app, id).await, ["render"], "queued once");
    assert!(run_job(&app, JobKind::Render).await);
    let (_, size) = rendered(&app, &thumbnail).await;
    assert_eq!(size, (256, 128));

    // Built with another representation version: the same, for a preview too.
    sqlx::query("UPDATE representations SET version = 0 WHERE blob_id = $1 AND kind = 'preview'")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    no_preview(&app, &preview, "pending", "preview is pending").await;
    assert_eq!(jobs_for(&app, id).await, ["render"]);
    // What the row said of the last render does not stay beside `pending`.
    let row: (Option<Vec<u8>>, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT sha256, size, source FROM representations WHERE blob_id = $1 AND kind = 'preview'",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    assert_eq!(row, (None, None, None));
    assert!(run_job(&app, JobKind::Render).await);
    rendered(&app, &preview).await;
    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM representations WHERE blob_id = $1 AND kind = 'preview'",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    assert_eq!(version, app.state.config().media.representation_version);

    // A failed representation is retried on a version bump as well.
    sqlx::query(
        "UPDATE representations SET state = 'failed', format = NULL, sha256 = NULL, size = NULL, \
         version = 0 \
         WHERE blob_id = $1 AND kind = 'thumbnail'",
    )
    .bind(parse_id(id))
    .execute(app.state.pool())
    .await
    .unwrap();
    icon(&app, &thumbnail, "pending").await;
    assert_eq!(jobs_for(&app, id).await, ["render"]);
}

/// `ready` promises a digest and a size to answer conditional requests
/// with, and nothing else may carry them; and `disabled` is never stored,
/// being the absence of the row.
#[tokio::test]
async fn the_row_holds_a_digest_and_a_size_with_ready_alone() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    render_all(&app).await;

    for statement in [
        "UPDATE representations SET format = NULL WHERE blob_id = $1 AND kind = 'thumbnail'",
        "UPDATE representations SET format = 'avif' WHERE blob_id = $1 AND kind = 'thumbnail'",
        "UPDATE representations SET sha256 = NULL WHERE blob_id = $1 AND kind = 'thumbnail'",
        "UPDATE representations SET size = NULL WHERE blob_id = $1 AND kind = 'thumbnail'",
        "UPDATE representations SET state = 'pending' WHERE blob_id = $1 AND kind = 'thumbnail'",
        "UPDATE representations SET sha256 = NULL WHERE blob_id = $1 AND kind = 'preview'",
        "UPDATE representations SET size = NULL WHERE blob_id = $1 AND kind = 'preview'",
        "UPDATE representations SET state = 'failed' WHERE blob_id = $1 AND kind = 'preview'",
        "UPDATE representations SET state = 'disabled', format = NULL, sha256 = NULL, size = NULL \
         WHERE blob_id = $1 AND kind = 'preview'",
    ] {
        let err = sqlx::query(statement)
            .bind(parse_id(id))
            .execute(app.state.pool())
            .await
            .expect_err(statement);
        assert!(
            err.as_database_error().unwrap().is_check_violation(),
            "{statement}: {err}"
        );
    }
}

#[tokio::test]
async fn a_corrupt_representation_is_an_internal_error() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    render_all(&app).await;
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    rendered(&app, &thumbnail).await;

    // An object that does not decrypt is repaired like a missing one: an
    // <img> keeps an image meanwhile.
    let corrupt = async |name: &str| {
        app.state
            .store()
            .put(
                &object_store::path::Path::from(object_key(id, name)),
                vec![0u8; 512].into(),
            )
            .await
            .unwrap();
    };
    corrupt("thumbnail").await;
    let (status, headers, _) = fetch(&app.private, get_with(&thumbnail, &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state_of(&headers), "pending");
    assert_eq!(jobs_for(&app, id).await, ["render"]);
    render_all(&app).await;
    rendered(&app, &thumbnail).await;

    // The thumbnail regenerated from a stored preview that does not decrypt:
    // the job renders the preview again rather than die on it.
    corrupt("preview").await;
    corrupt("thumbnail").await;
    let (_, headers, _) = fetch(&app.private, get_with(&thumbnail, &[])).await;
    assert_eq!(state_of(&headers), "pending");
    render_all(&app).await;
    rendered(&app, &thumbnail).await;
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["preview"]["state"], "ready");
    assert_eq!(blob["failed_at"], Value::Null);
}

#[tokio::test]
async fn representations_disabled_at_creation_are_never_rendered() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = fixture("rotated.jpg");
    let mut neither = blob_for(&bytes, "image/jpeg", "raw");
    neither["filename"] = json!("neither.jpg");
    neither["skip_virus_scan"] = json!(true);
    neither["representations"] = json!([]);
    let mut thumbnail_only = neither.clone();
    thumbnail_only["filename"] = json!("thumb.jpg");
    thumbnail_only["representations"] = json!(["thumbnail"]);
    let unknown = {
        let mut blob = neither.clone();
        blob["representations"] = json!(["icon"]);
        blob
    };
    let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", unknown)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let neither = create(&app, neither).await;
    let thumbnail_only = create(&app, thumbnail_only).await;
    for blob in [&neither, &thumbnail_only] {
        let id = id_of(blob);
        let (status, body) = put_private(&app, id, bytes.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    // Nothing to render: no job, and the icon and 404 are final. A
    // disabled kind has no row and no entry.
    let id = id_of(&neither);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["state"], "ready");
    assert_eq!(blob["representations"], json!({}), "{blob}");
    assert_eq!(representation_rows(&app, id).await, 0);
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());
    let thumbnail = format!("/blobs/{id}/thumbnail/neither.jpg");
    let preview = format!("/blobs/{id}/preview/neither.jpg");
    icon(&app, &thumbnail, "disabled").await;
    no_preview(&app, &preview, "disabled", "preview disabled for this blob").await;
    // The lazy repair path leaves a disabled representation alone, even
    // with the representation version it never had.
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());

    // The thumbnail alone is rendered, from the original: one row, one
    // entry in the metadata, and nothing of a preview.
    let id = id_of(&thumbnail_only);
    assert_eq!(representation_rows(&app, id).await, 1);
    assert_eq!(jobs_for(&app, id).await, ["render"]);
    render_all(&app).await;
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["representations"]["thumbnail"]["state"], "ready");
    assert!(
        blob["representations"]["thumbnail"]["checksum"].is_string(),
        "{blob}"
    );
    assert!(
        blob["representations"]["thumbnail"]["size"].is_u64(),
        "{blob}"
    );
    assert_eq!(
        blob["representations"].as_object().unwrap().len(),
        1,
        "{blob}"
    );
    assert_eq!(blob["representations"]["preview"], Value::Null);
    let (_, size) = rendered(&app, &format!("/blobs/{id}/thumbnail/thumb.jpg")).await;
    assert_eq!(size, (128, 256));
    no_preview(
        &app,
        &format!("/blobs/{id}/preview/thumb.jpg"),
        "disabled",
        "preview disabled for this blob",
    )
    .await;
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());
    let mut objects: Vec<String> = stored_objects(&app)
        .await
        .into_iter()
        .map(|o| o.location.to_string())
        .filter(|key| key.starts_with(&object_key(id, "")))
        .collect();
    objects.sort();
    assert_eq!(
        objects,
        [object_key(id, "original"), object_key(id, "thumbnail")]
    );
}

/// How many rows of `representations` the blob has.
async fn representation_rows(app: &TestApp, id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM representations WHERE blob_id = $1")
        .bind(parse_id(id))
        .fetch_one(app.state.pool())
        .await
        .unwrap()
}

/// A thumbnail regenerated on its own is made from the stored preview: the
/// original is never read, which deleting it proves.
#[tokio::test]
async fn thumbnails_regenerate_from_the_stored_preview() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    render_all(&app).await;
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    for object in ["thumbnail", "original"] {
        app.state
            .store()
            .delete(&object_store::path::Path::from(object_key(id, object)))
            .await
            .unwrap();
    }
    icon(&app, &thumbnail, "pending").await;
    assert!(run_job(&app, JobKind::Render).await);
    let (_, size) = rendered(&app, &thumbnail).await;
    assert_eq!(size, (256, 128));
    assert_eq!(
        get_blob(&app, id).await["representations"]["preview"]["state"],
        "ready"
    );
}

/// A preview rendered again over a `ready` row, for a thumbnail that
/// needs one it cannot use, goes back to `pending` first, as a request for
/// a stale or missing one sets it: a row never describes an object the
/// render is replacing. Seen with the render's record refused, as a
/// database that went away at that statement would: the row is `pending`
/// without a digest, not `ready` with the old digest over a new object.
#[tokio::test]
async fn a_preview_rendered_again_is_pending_meanwhile() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    render_all(&app).await;
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    let preview = format!("/blobs/{id}/preview/photo.jpg");

    // The preview's object gone and the thumbnail stale: the job renders
    // the preview again, over a `ready` row, for the thumbnail.
    app.state
        .store()
        .delete(&object_store::path::Path::from(object_key(id, "preview")))
        .await
        .unwrap();
    sqlx::query("UPDATE representations SET version = 0 WHERE blob_id = $1 AND kind = 'thumbnail'")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    icon(&app, &thumbnail, "pending").await;
    sqlx::raw_sql(
        "CREATE FUNCTION fail_ready_preview() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'the database went away'; END $$; \
         CREATE TRIGGER fail_ready_preview BEFORE UPDATE ON representations FOR EACH ROW \
         WHEN (NEW.kind = 'preview' AND NEW.state = 'ready') \
         EXECUTE FUNCTION fail_ready_preview();",
    )
    .execute(app.state.pool())
    .await
    .unwrap();
    assert!(run_job(&app, JobKind::Render).await);
    let row: (String, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT state, sha256 FROM representations WHERE blob_id = $1 AND kind = 'preview'",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    assert_eq!(row, ("pending".to_string(), None));
    let (status, headers, _) = fetch(&app.private, get_with(&preview, &[])).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(state_of(&headers), "pending");

    // The database back, the next attempt records both.
    sqlx::raw_sql(
        "DROP TRIGGER fail_ready_preview ON representations; \
         DROP FUNCTION fail_ready_preview();",
    )
    .execute(app.state.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'render'")
        .execute(app.state.pool())
        .await
        .unwrap();
    assert!(run_job(&app, JobKind::Render).await);
    rendered(&app, &preview).await;
    let (_, size) = rendered(&app, &thumbnail).await;
    assert_eq!(size, (256, 128));
}

/// A thumbnail regenerated on its own follows the verdict the preview has
/// from this version: none from a failed preview, the text sample for a
/// file with no preview, and a preview rendered again when its object is
/// gone.
#[tokio::test]
async fn thumbnails_regenerate_from_the_preview_verdict() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let broken = ready_file(&app, "broken.jpg", "image/jpeg", &fixture("truncated.jpg")).await;
    let notes = ready_file(&app, "notes.txt", "text/plain", b"Bonjour\n").await;
    let photo = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    render_all(&app).await;
    let photo_id = id_of(&photo);
    for object in ["thumbnail", "preview"] {
        app.state
            .store()
            .delete(&object_store::path::Path::from(object_key(
                photo_id, object,
            )))
            .await
            .unwrap();
    }

    for (blob, preview, thumbnail) in [
        (&broken, "failed", "failed"),
        (&notes, "unavailable", "ready"),
        (&photo, "ready", "ready"),
    ] {
        let id = id_of(blob);
        let name = blob["filename"].as_str().unwrap();
        sqlx::query(
            "UPDATE representations SET version = 0 WHERE blob_id = $1 AND kind = 'thumbnail'",
        )
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
        icon(&app, &format!("/blobs/{id}/thumbnail/{name}"), "pending").await;
        assert!(run_job(&app, JobKind::Render).await, "{name}");
        let got = get_blob(&app, id).await;
        assert_eq!(
            got["representations"]["preview"]["state"], preview,
            "{name}"
        );
        assert_eq!(
            got["representations"]["thumbnail"]["state"], thumbnail,
            "{name}"
        );
    }
    rendered(&app, &format!("/blobs/{photo_id}/thumbnail/photo.jpg")).await;
    rendered(&app, &format!("/blobs/{photo_id}/preview/photo.jpg")).await;
}

/// A render job that finds nothing to do, because the blob is no longer
/// readable or nothing is pending, finishes without touching it.
#[tokio::test]
async fn render_jobs_with_nothing_to_do_finish() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let photo = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&photo);
    render_all(&app).await;
    let rendered_at: String =
        sqlx::query_scalar("SELECT updated_at::text FROM blobs WHERE id = $1")
            .bind(parse_id(id))
            .fetch_one(app.state.pool())
            .await
            .unwrap();
    let enqueue = || async {
        sqlx::query("INSERT INTO jobs (kind, blob_id) VALUES ('render', $1)")
            .bind(parse_id(id))
            .execute(app.state.pool())
            .await
            .unwrap();
    };

    enqueue().await;
    assert!(run_job(&app, JobKind::Render).await);
    let unchanged: String = sqlx::query_scalar("SELECT updated_at::text FROM blobs WHERE id = $1")
        .bind(parse_id(id))
        .fetch_one(app.state.pool())
        .await
        .unwrap();
    assert_eq!(unchanged, rendered_at, "nothing pending, nothing written");

    let pending = ready_file(&app, "again.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    assert_eq!(delete(&app, id_of(&pending)).await, StatusCode::NO_CONTENT);
    assert!(run_job(&app, JobKind::Render).await);
    let got = get_blob(&app, id_of(&pending)).await;
    assert_eq!(got["state"], "deleted");
    assert_eq!(
        got["representations"]["thumbnail"]["state"], "pending",
        "never rendered"
    );
    assert_eq!(jobs_for(&app, id_of(&pending)).await, ["purge"]);
}

#[tokio::test]
async fn waiting_for_a_pending_representation() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    let (status, minted) = call(
        &app.private,
        json_req(
            Method::POST,
            &format!("/blobs/{id}/urls"),
            json!({"query": {"wait": "5000"}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{minted}");
    let thumbnail = public_path(minted["thumbnail"].as_str().unwrap());
    assert!(thumbnail.contains("wait=5000"), "{thumbnail}");

    // The render lands while the request waits.
    let state = app.state.clone();
    let job = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        agrafe::jobs::runner::run_one(&state, JobKind::Render, "late")
            .await
            .unwrap()
    });
    let started = Instant::now();
    let (status, headers, body) = fetch(&app.public, get_with(&thumbnail, &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state_of(&headers), "ready");
    assert_eq!(webp_size(&body), (256, 128));
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert!(within("the render job", job).await.unwrap());

    // Without `wait`, the placeholder comes back at once.
    let blob = ready_file(&app, "other.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    let started = Instant::now();
    no_preview(
        &app,
        &format!("/blobs/{id}/preview/other.jpg"),
        "pending",
        "preview is pending",
    )
    .await;
    assert!(started.elapsed() < Duration::from_millis(200));

    // A wait that runs out answers the placeholder.
    let started = Instant::now();
    let (status, headers, _) = fetch(
        &app.private,
        get_with(&format!("/blobs/{id}/preview/other.jpg?wait=300"), &[]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(state_of(&headers), "pending");
    assert!(started.elapsed() >= Duration::from_millis(300));
}
