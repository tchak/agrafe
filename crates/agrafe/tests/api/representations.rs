//! Thumbnails and previews: rendering from images, PDFs and office files,
//! the placeholders served meanwhile, waiting, and regeneration.

use agrafe::jobs::JobKind;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use object_store::ObjectStoreExt;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

use crate::support::*;

/// Run the thumbnail and preview jobs for every blob that has them queued.
async fn render_all(app: &TestApp) {
    while run_job(app, JobKind::Thumbnail).await {}
    while run_job(app, JobKind::Preview).await {}
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
    let app = setup().await;
    // 600x300 with EXIF orientation 6: upright it is 300x600.
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("rotated.jpg")).await;
    let id = id_of(&blob);
    assert_eq!(blob["thumbnail_state"], "pending");
    assert_eq!(blob["preview_state"], "pending");
    assert_eq!(jobs_for(&app, id).await, ["preview", "thumbnail"]);
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    let preview = format!("/blobs/{id}/preview/photo.jpg");

    // Until the jobs run: the icon, not to be cached, and no preview.
    let placeholder = icon(&app, &thumbnail, "pending").await;
    no_preview(&app, &preview, "pending", "preview is pending").await;

    assert!(run_job(&app, JobKind::Thumbnail).await);
    assert!(run_job(&app, JobKind::Preview).await);
    assert!(!run_job(&app, JobKind::Thumbnail).await);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["thumbnail_state"], "ready");
    assert_eq!(blob["preview_state"], "ready");
    assert_eq!(blob["preview_source"], "image");

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
            format!("{id}/original"),
            format!("{id}/preview"),
            format!("{id}/thumbnail")
        ]
    );
    let stored = app
        .state
        .store()
        .get(&object_store::path::Path::from(format!("{id}/thumbnail")))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_ne!(&stored[..4], b"RIFF");
}

#[tokio::test]
async fn vector_images_fill_the_box() {
    let app = setup().await;
    // 20x10 nominal: a raster that small would stay small.
    let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="red"/></svg>"#;
    let blob = ready_file(&app, "logo.svg", "image/svg+xml", svg).await;
    let id = id_of(&blob);
    render_all(&app).await;
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["preview_source"], "image");
    let (_, size) = rendered(&app, &format!("/blobs/{id}/thumbnail/logo.svg")).await;
    assert_eq!(size, (256, 128));
    let (_, size) = rendered(&app, &format!("/blobs/{id}/preview/logo.svg")).await;
    assert_eq!(size, (400, 200));
}

#[tokio::test]
async fn pdf_previews_come_from_the_embedded_image_or_the_page() {
    let app = setup().await;
    // One JPEG covering a page a tenth of its size: the image itself is the
    // preview, at its own 600x300 fitted into 400.
    let wrapped = ready_file(&app, "scan.pdf", "application/pdf", &fixture("image.pdf")).await;
    // The same image with a paragraph beside it: the page is rendered, and
    // being vector it fills the box whatever its nominal size.
    let mixed = ready_file(&app, "mixed.pdf", "application/pdf", &fixture("mixed.pdf")).await;
    let text = ready_file(&app, "text.pdf", "application/pdf", &fixture("text.pdf")).await;
    render_all(&app).await;

    for (blob, source, preview_size, thumbnail_size) in [
        (&wrapped, "embedded_image", (400, 200), (256, 128)),
        (&mixed, "first_page", (400, 200), (256, 128)),
        (&text, "first_page", (400, 200), (256, 128)),
    ] {
        let id = id_of(blob);
        let name = blob["filename"].as_str().unwrap();
        let blob = get_blob(&app, id).await;
        assert_eq!(blob["preview_state"], "ready", "{name}");
        assert_eq!(blob["preview_source"], source, "{name}");
        assert_eq!(blob["thumbnail_state"], "ready", "{name}");
        let (_, size) = rendered(&app, &format!("/blobs/{id}/preview/{name}")).await;
        assert_eq!(size, preview_size, "{name}");
        let (_, size) = rendered(&app, &format!("/blobs/{id}/thumbnail/{name}")).await;
        assert_eq!(size, thumbnail_size, "{name}");
    }
}

#[tokio::test]
async fn office_documents_wrapping_a_picture() {
    let app = setup().await;
    let docx = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
    let odt = "application/vnd.oasis.opendocument.text";
    let picture = ready_file(&app, "picture.docx", docx, &fixture("image.docx")).await;
    let letter = ready_file(&app, "letter.docx", docx, &fixture("text.docx")).await;
    let odt_picture = ready_file(&app, "picture.odt", odt, &fixture("image.odt")).await;
    render_all(&app).await;

    for blob in [&picture, &odt_picture] {
        let id = id_of(blob);
        let name = blob["filename"].as_str().unwrap();
        let blob = get_blob(&app, id).await;
        assert_eq!(blob["preview_source"], "embedded_image", "{name}");
        let (_, size) = rendered(&app, &format!("/blobs/{id}/preview/{name}")).await;
        assert_eq!(size, (64, 32), "{name}");
        let (_, size) = rendered(&app, &format!("/blobs/{id}/thumbnail/{name}")).await;
        assert_eq!(size, (64, 32), "{name}");
    }

    // A real document has no renderer yet: no preview, and the icon stays.
    let id = id_of(&letter);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["preview_state"], "unavailable");
    assert_eq!(blob["preview_source"], Value::Null);
    assert_eq!(blob["thumbnail_state"], "unavailable");
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

#[tokio::test]
async fn text_files_get_a_thumbnail_and_no_preview() {
    let app = setup().await;
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
        assert_eq!(blob["thumbnail_state"], "ready", "{name}");
        assert_eq!(blob["preview_state"], "unavailable", "{name}");
        assert_eq!(blob["preview_source"], Value::Null, "{name}");
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
    assert_eq!(get_blob(&app, id).await["thumbnail_state"], "unavailable");
    icon(
        &app,
        &format!("/blobs/{id}/thumbnail/fake.txt"),
        "unavailable",
    )
    .await;
}

#[tokio::test]
async fn files_the_renderer_rejects_are_failed() {
    let app = setup().await;
    let blob = ready_file(&app, "broken.jpg", "image/jpeg", &fixture("truncated.jpg")).await;
    let id = id_of(&blob);
    render_all(&app).await;
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["thumbnail_state"], "failed");
    assert_eq!(blob["preview_state"], "failed");
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
    assert_eq!(blob["thumbnail_state"], "failed");
    assert_eq!(blob["preview_state"], "failed");
}

#[tokio::test]
async fn oversized_originals_get_no_representations() {
    let clamd = fake_clamd().await;
    let app = setup_tuned(clamd, |c| {
        c.media.max_source_size = bytesize::ByteSize::b(1000)
    })
    .await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    render_all(&app).await;
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["thumbnail_state"], "unavailable");
    assert_eq!(blob["preview_state"], "unavailable");
}

#[tokio::test]
async fn a_missing_renderer_fails_the_job_not_the_file() {
    let clamd = fake_clamd().await;
    let app = setup_tuned(clamd, |c| c.media.vips_command = "/nonexistent/vips".into()).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    assert!(run_job(&app, JobKind::Thumbnail).await);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["thumbnail_state"], "pending");
    let error: String =
        sqlx::query_scalar("SELECT last_error FROM jobs WHERE blob_id = $1 AND kind = 'thumbnail'")
            .bind(parse_id(id))
            .fetch_one(app.state.pool())
            .await
            .unwrap();
    assert!(error.contains("starting `/nonexistent/vips`"), "{error}");
}

#[tokio::test]
async fn blob_state_governs_the_representations() {
    let app = setup().await;

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
    // and no regeneration is queued for a blob on its way out.
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
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());
}

#[tokio::test]
async fn representations_regenerate_when_missing_or_stale() {
    let app = setup().await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    render_all(&app).await;
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    let preview = format!("/blobs/{id}/preview/photo.jpg");

    // The object vanished from the store: back to pending, queued again.
    app.state
        .store()
        .delete(&object_store::path::Path::from(format!("{id}/thumbnail")))
        .await
        .unwrap();
    icon(&app, &thumbnail, "pending").await;
    assert_eq!(get_blob(&app, id).await["thumbnail_state"], "pending");
    assert_eq!(jobs_for(&app, id).await, ["thumbnail"]);
    icon(&app, &thumbnail, "pending").await;
    assert_eq!(jobs_for(&app, id).await, ["thumbnail"], "queued once");
    assert!(run_job(&app, JobKind::Thumbnail).await);
    let (_, size) = rendered(&app, &thumbnail).await;
    assert_eq!(size, (256, 128));

    // Built with another variant version: the same, for a preview too.
    sqlx::query("UPDATE blobs SET preview_variant_version = 0 WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    no_preview(&app, &preview, "pending", "preview is pending").await;
    assert_eq!(jobs_for(&app, id).await, ["preview"]);
    assert!(run_job(&app, JobKind::Preview).await);
    rendered(&app, &preview).await;
    let version: i32 =
        sqlx::query_scalar("SELECT preview_variant_version FROM blobs WHERE id = $1")
            .bind(parse_id(id))
            .fetch_one(app.state.pool())
            .await
            .unwrap();
    assert_eq!(version, app.state.config().media.variant_version);

    // A failed representation is retried on a version bump as well.
    sqlx::query(
        "UPDATE blobs SET thumbnail_state = 'failed', thumbnail_variant_version = 0 WHERE id = $1",
    )
    .bind(parse_id(id))
    .execute(app.state.pool())
    .await
    .unwrap();
    icon(&app, &thumbnail, "pending").await;
    assert_eq!(jobs_for(&app, id).await, ["thumbnail"]);
}

#[tokio::test]
async fn waiting_for_a_pending_representation() {
    let app = setup().await;
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
    assert!(thumbnail.ends_with("?wait=5000"), "{thumbnail}");

    // The render lands while the request waits.
    let state = app.state.clone();
    let job = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        agrafe::jobs::runner::run_one(&state, JobKind::Thumbnail, "late")
            .await
            .unwrap()
    });
    let started = Instant::now();
    let (status, headers, body) = fetch(&app.public, get_with(&thumbnail, &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state_of(&headers), "ready");
    assert_eq!(webp_size(&body), (256, 128));
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert!(job.await.unwrap());

    // Without `wait`, the placeholder comes back at once.
    let started = Instant::now();
    no_preview(
        &app,
        &format!("/blobs/{id}/preview/photo.jpg"),
        "pending",
        "preview is pending",
    )
    .await;
    assert!(started.elapsed() < Duration::from_millis(200));

    // A wait that runs out answers the placeholder.
    let started = Instant::now();
    let (status, headers, _) = fetch(
        &app.private,
        get_with(&format!("/blobs/{id}/preview/photo.jpg?wait=300"), &[]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(state_of(&headers), "pending");
    assert!(started.elapsed() >= Duration::from_millis(300));
}
