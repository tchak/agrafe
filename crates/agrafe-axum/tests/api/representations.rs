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

#[sqlx::test(migrator = "MIGRATOR")]
async fn image_gets_a_thumbnail_and_a_preview(pool: PgPool) {
    let app = setup(pool).await;
    // 600x300 with EXIF orientation 6: upright it is 300x600.
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("rotated.jpg")).await;
    let id = id_of(&blob);
    assert_eq!(blob["thumbnail_state"], "pending");
    assert_eq!(blob["preview_state"], "pending");
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

#[sqlx::test(migrator = "MIGRATOR")]
async fn vector_images_fill_the_box(pool: PgPool) {
    let app = setup(pool).await;
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

#[sqlx::test(migrator = "MIGRATOR")]
async fn pdf_previews_come_from_the_embedded_image_only(pool: PgPool) {
    let app = setup(pool).await;
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
    assert_eq!(blob["preview_state"], "ready");
    assert_eq!(blob["preview_source"], "embedded_image");
    assert_eq!(blob["thumbnail_state"], "ready");
    let (_, size) = rendered(&app, &format!("/blobs/{id}/preview/scan.pdf")).await;
    assert_eq!(size, (400, 200));
    let (_, size) = rendered(&app, &format!("/blobs/{id}/thumbnail/scan.pdf")).await;
    assert_eq!(size, (256, 128));

    for blob in [&mixed, &text] {
        let id = id_of(blob);
        let name = blob["filename"].as_str().unwrap();
        let blob = get_blob(&app, id).await;
        assert_eq!(blob["preview_state"], "unavailable", "{name}");
        assert_eq!(blob["preview_source"], Value::Null, "{name}");
        assert_eq!(blob["thumbnail_state"], "ready", "{name}");
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

#[sqlx::test(migrator = "MIGRATOR")]
async fn office_documents_wrapping_a_picture(pool: PgPool) {
    let app = setup(pool).await;
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

    // A real document with no converter configured: no preview, and the
    // icon stays.
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

const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const ODT: &str = "application/vnd.oasis.opendocument.text";
const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const ODS: &str = "application/vnd.oasis.opendocument.spreadsheet";

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

#[sqlx::test(migrator = "MIGRATOR")]
async fn office_documents_get_their_first_page_through_libreoffice(pool: PgPool) {
    let Some(soffice) = soffice() else {
        return;
    };
    let clamd = fake_clamd().await;
    let app = setup_tuned(pool, clamd, |c| {
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
        assert_eq!(blob["preview_state"], "unavailable", "{name}");
        assert_eq!(blob["thumbnail_state"], "ready", "{name}");
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
    assert_eq!(blob["preview_source"], "embedded_image");
    assert_eq!(blob["thumbnail_state"], "ready");
    let id = id_of(&broken);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["thumbnail_state"], "failed");
    icon(
        &app,
        &format!("/blobs/{id}/thumbnail/broken.docx"),
        "failed",
    )
    .await;
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_startup_probe_checks_libreoffice(pool: PgPool) {
    let app = setup(pool).await;
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

#[sqlx::test(migrator = "MIGRATOR")]
async fn text_files_get_a_thumbnail_and_no_preview(pool: PgPool) {
    let app = setup(pool).await;
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

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_crashing_helper_fails_the_file_not_the_job(pool: PgPool) {
    let clamd = fake_clamd().await;
    let app = setup_tuned(pool, clamd, |c| {
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
        assert_eq!(blob["thumbnail_state"], "failed", "{name}");
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
    for (blob, name, preview) in [
        (&scan, "scan.pdf", "failed"),
        (&thumbnail_only, "thumb.pdf", "disabled"),
    ] {
        let id = id_of(blob);
        let blob = get_blob(&app, id).await;
        assert_eq!(blob["preview_state"], preview, "{name}");
        assert_eq!(blob["thumbnail_state"], "ready", "{name}");
        rendered(&app, &format!("/blobs/{id}/thumbnail/{name}")).await;
    }
}

/// A file that exhausts a tool's time limit is not handed to the next one
/// for as long again: the PDF's first page is not tried once the
/// extractor timed out on it, and both representations are failed.
#[sqlx::test(migrator = "MIGRATOR")]
async fn a_timed_out_helper_fails_both_representations(pool: PgPool) {
    use std::os::unix::fs::PermissionsExt;
    let slow = std::env::temp_dir().join(format!("agrafe-slow-extract-{}", uuid::Uuid::now_v7()));
    std::fs::write(&slow, "#!/bin/sh\nsleep 5\n").unwrap();
    std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    let clamd = fake_clamd().await;
    let app = setup_tuned(pool, clamd, |c| {
        c.media.render.extract_command = slow.to_string_lossy().into_owned();
        c.media.render.sandbox.timeout = std::time::Duration::from_millis(300);
    })
    .await;
    let scan = ready_file(&app, "scan.pdf", "application/pdf", &fixture("image.pdf")).await;
    render_all(&app).await;
    let id = id_of(&scan);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["preview_state"], "failed");
    assert_eq!(blob["thumbnail_state"], "failed");
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());
    std::fs::remove_file(&slow).unwrap();
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn files_the_renderer_rejects_are_failed(pool: PgPool) {
    let app = setup(pool).await;
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

#[sqlx::test(migrator = "MIGRATOR")]
async fn oversized_originals_get_no_representations(pool: PgPool) {
    let clamd = fake_clamd().await;
    let app = setup_tuned(pool, clamd, |c| {
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
    assert_eq!(blob["thumbnail_state"], "unavailable");
    assert_eq!(blob["preview_state"], "unavailable");
    let text = get_blob(&app, id_of(&text)).await;
    assert_eq!(text["thumbnail_state"], "ready");
    assert_eq!(text["preview_state"], "unavailable");
    let pdf = get_blob(&app, id_of(&pdf)).await;
    assert_eq!(pdf["thumbnail_state"], "unavailable");
    assert_eq!(pdf["preview_state"], "unavailable");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_missing_renderer_fails_the_job_not_the_file(pool: PgPool) {
    let clamd = fake_clamd().await;
    let app = setup_tuned(pool, clamd, |c| {
        c.media.render.vips_command = "/nonexistent/vips".into()
    })
    .await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    assert!(run_job(&app, JobKind::Render).await);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["thumbnail_state"], "pending");
    assert_eq!(blob["preview_state"], "pending");
    let error: String =
        sqlx::query_scalar("SELECT last_error FROM jobs WHERE blob_id = $1 AND kind = 'render'")
            .bind(parse_id(id))
            .fetch_one(app.state.pool())
            .await
            .unwrap();
    assert!(error.contains("starting `/nonexistent/vips`"), "{error}");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_startup_probe_checks_vips_and_the_helper(pool: PgPool) {
    let app = setup(pool).await;
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
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn blob_state_governs_the_representations(pool: PgPool) {
    let app = setup(pool).await;

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

#[sqlx::test(migrator = "MIGRATOR")]
async fn conditional_and_head_requests_are_answered_from_the_row(pool: PgPool) {
    let app = setup(pool).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    render_all(&app).await;
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    let (headers, _) = rendered(&app, &thumbnail).await;
    let etag = headers["etag"].to_str().unwrap().to_string();
    let length = headers["content-length"].clone();

    // The ETag is the digest the row records: the one the bytes have.
    let (digest, size): (Vec<u8>, i32) =
        sqlx::query_as("SELECT thumbnail_sha256, thumbnail_size FROM blobs WHERE id = $1")
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
        .delete(&object_store::path::Path::from(format!("{id}/thumbnail")))
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

#[sqlx::test(migrator = "MIGRATOR")]
async fn representations_regenerate_when_missing_or_stale(pool: PgPool) {
    let app = setup(pool).await;
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
    assert_eq!(jobs_for(&app, id).await, ["render"]);
    icon(&app, &thumbnail, "pending").await;
    assert_eq!(jobs_for(&app, id).await, ["render"], "queued once");
    assert!(run_job(&app, JobKind::Render).await);
    let (_, size) = rendered(&app, &thumbnail).await;
    assert_eq!(size, (256, 128));

    // Built with another variant version: the same, for a preview too.
    sqlx::query("UPDATE blobs SET preview_variant_version = 0 WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    no_preview(&app, &preview, "pending", "preview is pending").await;
    assert_eq!(jobs_for(&app, id).await, ["render"]);
    assert!(run_job(&app, JobKind::Render).await);
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
    assert_eq!(jobs_for(&app, id).await, ["render"]);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn representations_disabled_at_creation_are_never_rendered(pool: PgPool) {
    let app = setup(pool).await;
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

    // Nothing to render: no job, and the icon and 404 are final.
    let id = id_of(&neither);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["state"], "ready");
    assert_eq!(blob["thumbnail_state"], "disabled");
    assert_eq!(blob["preview_state"], "disabled");
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());
    let thumbnail = format!("/blobs/{id}/thumbnail/neither.jpg");
    let preview = format!("/blobs/{id}/preview/neither.jpg");
    icon(&app, &thumbnail, "disabled").await;
    no_preview(&app, &preview, "disabled", "preview disabled for this blob").await;
    // The lazy repair path leaves a disabled representation alone, even
    // with the variant version it never had.
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());

    // The thumbnail alone is rendered, from the original.
    let id = id_of(&thumbnail_only);
    assert_eq!(jobs_for(&app, id).await, ["render"]);
    render_all(&app).await;
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["thumbnail_state"], "ready");
    assert_eq!(blob["preview_state"], "disabled");
    assert_eq!(blob["preview_source"], Value::Null);
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
        .filter(|key| key.starts_with(id))
        .collect();
    objects.sort();
    assert_eq!(
        objects,
        [format!("{id}/original"), format!("{id}/thumbnail")]
    );
}

/// A thumbnail regenerated on its own is made from the stored preview: the
/// original is never read, which deleting it proves.
#[sqlx::test(migrator = "MIGRATOR")]
async fn thumbnails_regenerate_from_the_stored_preview(pool: PgPool) {
    let app = setup(pool).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    render_all(&app).await;
    let thumbnail = format!("/blobs/{id}/thumbnail/photo.jpg");
    for object in ["thumbnail", "original"] {
        app.state
            .store()
            .delete(&object_store::path::Path::from(format!("{id}/{object}")))
            .await
            .unwrap();
    }
    icon(&app, &thumbnail, "pending").await;
    assert!(run_job(&app, JobKind::Render).await);
    let (_, size) = rendered(&app, &thumbnail).await;
    assert_eq!(size, (256, 128));
    assert_eq!(get_blob(&app, id).await["preview_state"], "ready");
}

/// A thumbnail regenerated on its own follows the verdict the preview has
/// from this version: none from a failed preview, the text sample for a
/// file with no preview, and a preview rendered again when its object is
/// gone.
#[sqlx::test(migrator = "MIGRATOR")]
async fn thumbnails_regenerate_from_the_preview_verdict(pool: PgPool) {
    let app = setup(pool).await;
    let broken = ready_file(&app, "broken.jpg", "image/jpeg", &fixture("truncated.jpg")).await;
    let notes = ready_file(&app, "notes.txt", "text/plain", b"Bonjour\n").await;
    let photo = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    render_all(&app).await;
    let photo_id = id_of(&photo);
    for object in ["thumbnail", "preview"] {
        app.state
            .store()
            .delete(&object_store::path::Path::from(format!(
                "{photo_id}/{object}"
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
        sqlx::query("UPDATE blobs SET thumbnail_variant_version = 0 WHERE id = $1")
            .bind(parse_id(id))
            .execute(app.state.pool())
            .await
            .unwrap();
        icon(&app, &format!("/blobs/{id}/thumbnail/{name}"), "pending").await;
        assert!(run_job(&app, JobKind::Render).await, "{name}");
        let got = get_blob(&app, id).await;
        assert_eq!(got["preview_state"], preview, "{name}");
        assert_eq!(got["thumbnail_state"], thumbnail, "{name}");
    }
    rendered(&app, &format!("/blobs/{photo_id}/thumbnail/photo.jpg")).await;
    rendered(&app, &format!("/blobs/{photo_id}/preview/photo.jpg")).await;
}

/// A render job that finds nothing to do, because the blob is no longer
/// readable or nothing is pending, finishes without touching it.
#[sqlx::test(migrator = "MIGRATOR")]
async fn render_jobs_with_nothing_to_do_finish(pool: PgPool) {
    let app = setup(pool).await;
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
    assert_eq!(got["thumbnail_state"], "pending", "never rendered");
    assert_eq!(jobs_for(&app, id_of(&pending)).await, ["purge"]);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn waiting_for_a_pending_representation(pool: PgPool) {
    let app = setup(pool).await;
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
