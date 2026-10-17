//! Generated archives: the manifest accepted in parts, the finish, the
//! build job streaming members into a ZIP, and what happens to members and
//! sources that are abandoned on the way.

use std::io::{Cursor, Read};

use agrafe::jobs::JobKind;
use agrafe::jobs::reap;
use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jiff::Timestamp;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::support::*;

fn archive_blob(filename: &str) -> Value {
    json!({
        "filename": filename,
        "content_type": "application/zip",
        "input_type": "archive",
    })
}

/// A manifest of `(path, blob id)` lines.
fn manifest(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, id) in entries {
        out.extend_from_slice(json!({"path": path, "blob_id": id}).to_string().as_bytes());
        out.push(b'\n');
    }
    out
}

async fn put_part(app: &TestApp, id: &str, n: u32, bytes: Vec<u8>) -> (StatusCode, Value) {
    call(
        &app.private,
        put_raw(&format!("/blobs/{id}/parts/{n}"), bytes),
    )
    .await
}

async fn finish(app: &TestApp, id: &str) -> (StatusCode, Value) {
    call(
        &app.private,
        req(Method::POST, &format!("/blobs/{id}/finish")),
    )
    .await
}

/// The original of `blob`, downloaded whole through the private port.
async fn download(app: &TestApp, blob: &Value) -> Vec<u8> {
    let id = id_of(blob);
    let filename = agrafe::blob::urls::encode_segment(blob["filename"].as_str().unwrap());
    let (status, headers, bytes) = fetch(
        &app.private,
        get_with(&format!("/blobs/{id}/original/{filename}"), &[]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/zip");
    bytes
}

/// Every entry of a ZIP, read back by an independent implementation, which
/// checks each CRC on the way.
fn unzip(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).expect("a readable ZIP");
    (0..archive.len())
        .map(|i| {
            let mut file = archive.by_index(i).unwrap();
            assert_eq!(file.compression(), zip::CompressionMethod::Stored);
            let mut data = Vec::new();
            file.read_to_end(&mut data).unwrap();
            (file.name().unwrap().into_owned(), data)
        })
        .collect()
}

/// The source objects of a blob in the store.
async fn source_objects(app: &TestApp, id: &str) -> Vec<String> {
    let prefix = format!("{id}/source/");
    let mut keys: Vec<String> = stored_objects(app)
        .await
        .into_iter()
        .map(|o| o.location.to_string())
        .filter(|k| k.starts_with(&prefix))
        .collect();
    keys.sort();
    keys
}

#[tokio::test]
async fn archive_is_built_from_a_manifest_in_parts() {
    let app = setup().await;
    let report = ready_file(&app, "Report.pdf", "application/pdf", &fixture("text.pdf")).await;
    let photo = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let empty = ready_file(&app, "empty.txt", "text/plain", b"").await;
    let (report_id, photo_id, empty_id) = (id_of(&report), id_of(&photo), id_of(&empty));

    let created = create(&app, archive_blob("export été.zip")).await;
    let id = id_of(&created);
    assert_eq!(created["state"], "created");
    assert_eq!(
        created["skip_virus_scan"], true,
        "generated: by construction"
    );
    assert_eq!(created["scan_state"], "skipped");
    assert_eq!(created["source_parts"], 0);
    assert_eq!(created["source_rows"], 0);
    assert!(created["size"].is_null());
    assert!(created["archive_entries"].is_null());

    // Two parts; the same member under two paths; two paths that collide
    // on a case-insensitive file system.
    let (status, body) = put_part(
        &app,
        id,
        1,
        manifest(&[
            ("docs/Report.pdf", report_id),
            ("docs/report.pdf", photo_id),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "created");
    assert_eq!(body["source_parts"], 1);
    assert_eq!(body["source_rows"], 2);
    let after_one = body["size"].as_u64().unwrap();
    assert!(after_one > fixture("text.pdf").len() as u64 + fixture("photo.jpg").len() as u64);
    assert_eq!(
        source_objects(&app, id).await,
        vec![format!("{id}/source/1")]
    );

    let (status, body) = put_part(
        &app,
        id,
        2,
        manifest(&[("photos/été.jpg", photo_id), ("empty.txt", empty_id)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source_parts"], 2);
    assert_eq!(body["source_rows"], 4);
    assert!(body["size"].as_u64().unwrap() > after_one);
    assert!(body["source_size"].as_u64().unwrap() > 0);
    assert!(
        jobs_for(&app, id).await.is_empty(),
        "nothing until the finish"
    );

    let (status, body) = finish(&app, id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert!(body["uploaded_at"].is_string());
    assert_eq!(jobs_for(&app, id).await, vec!["archive"]);

    // Not readable until built.
    let (status, _, _) = fetch(
        &app.private,
        get_with(
            &format!("/blobs/{id}/original/export%20%C3%A9t%C3%A9.zip"),
            &[],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    assert!(run_job(&app, JobKind::Archive).await);
    let built = get_blob(&app, id).await;
    assert_eq!(built["state"], "ready", "{built}");
    assert_eq!(built["archive_entries"], 4);
    assert_eq!(built["archive_errors"], 0);
    assert_eq!(jobs_for(&app, id).await, vec!["render"]);

    let bytes = download(&app, &built).await;
    // For checking the file with other tools: `AGRAFE_ARCHIVE_OUT=/tmp/x.zip`.
    if let Ok(path) = std::env::var("AGRAFE_ARCHIVE_OUT") {
        std::fs::write(path, &bytes).unwrap();
    }
    assert_eq!(built["size"], bytes.len());
    assert_eq!(
        built["checksum"],
        STANDARD.encode(Sha256::digest(&bytes)),
        "the checksum is of the archive"
    );
    assert_eq!(
        storage_key(&app, id).await,
        format!("{id}/original"),
        "the source parts stay under the prefix until the purge"
    );
    let entries = unzip(&bytes);
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "docs/Report.pdf",
            "docs/report-1.pdf",
            "photos/été.jpg",
            "empty.txt"
        ],
        "manifest order, first occurrence keeps its name"
    );
    assert_eq!(entries[0].1, fixture("text.pdf"));
    assert_eq!(entries[1].1, fixture("photo.jpg"));
    assert_eq!(entries[2].1, fixture("photo.jpg"));
    assert!(entries[3].1.is_empty());

    // The archive gets its icon and no preview, without the renderer
    // touching the original. The members have renders queued too.
    while run_job(&app, JobKind::Render).await {}
    let rendered = get_blob(&app, id).await;
    assert_eq!(rendered["preview_state"], "unavailable");
    assert_eq!(rendered["thumbnail_state"], "unavailable");
}

/// `PUT /blobs/:id` is part one and the finish in one call.
#[tokio::test]
async fn whole_source_in_one_put() {
    let app = setup().await;
    let member = ready_blob(&app, &png_bytes(3000)).await;
    let created = create(&app, archive_blob("one.zip")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, manifest(&[("a.png", id_of(&member))])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert_eq!(body["source_parts"], 1);
    assert_eq!(jobs_for(&app, id).await, vec!["archive"]);
    assert!(run_job(&app, JobKind::Archive).await);
    let built = get_blob(&app, id).await;
    assert_eq!(built["state"], "ready");
    let entries = unzip(&download(&app, &built).await);
    assert_eq!(entries, vec![("a.png".to_string(), png_bytes(3000))]);
}

/// An empty manifest is a valid source and builds an empty archive.
#[tokio::test]
async fn empty_manifest() {
    let app = setup().await;
    let created = create(&app, archive_blob("nothing.zip")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, vec![]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source_rows"], 0);
    assert!(run_job(&app, JobKind::Archive).await);
    let built = get_blob(&app, id).await;
    assert_eq!(built["state"], "ready");
    assert_eq!(built["archive_entries"], 0);
    assert_eq!(
        built["size"], 22,
        "an end of central directory record alone"
    );
    assert!(unzip(&download(&app, &built).await).is_empty());
}

/// Every bad line answers 422 naming it, the part is discarded, the blob
/// is back in `created` with the parts it had, and nothing of the part is
/// in the store.
#[tokio::test]
async fn manifest_rejections() {
    let app = setup().await;
    let good = ready_blob(&app, &png_bytes(100)).await;
    let good_id = id_of(&good).to_string();
    let pending = create(&app, blob_for(&png_bytes(100), "image/png", "raw")).await;
    let infected = create(&app, blob_for(&infected_png(1500), "image/png", "raw")).await;
    let (status, _) = put_private(&app, id_of(&infected), infected_png(1500)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let deleted = ready_blob(&app, &png_bytes(100)).await;
    assert_eq!(delete(&app, id_of(&deleted)).await, StatusCode::NO_CONTENT);
    let unknown = uuid::Uuid::now_v7().to_string();

    let line = |path: &str, id: &str| json!({"path": path, "blob_id": id}).to_string();
    let g = good_id.as_str();
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("not json", b"nope\n".to_vec(), "line 1"),
        ("not an object", b"[1]\n".to_vec(), "line 1"),
        ("missing field", b"{\"path\": \"a\"}\n".to_vec(), "blob_id"),
        (
            "unknown field",
            format!("{{\"path\": \"a\", \"blob_id\": \"{g}\", \"x\": 1}}\n").into_bytes(),
            "line 1",
        ),
        (
            "no trailing newline",
            line("a.png", g).into_bytes(),
            "newline",
        ),
        (
            "second line bad",
            format!("{}\n{}\n", line("a.png", g), line("../b.png", g)).into_bytes(),
            "line 2",
        ),
        (
            "absolute",
            format!("{}\n", line("/a", g)).into_bytes(),
            "relative",
        ),
        (
            "backslash",
            format!("{}\n", line("a\\b", g)).into_bytes(),
            "backslash",
        ),
        (
            "empty path",
            format!("{}\n", line("", g)).into_bytes(),
            "empty",
        ),
        (
            "trailing slash",
            format!("{}\n", line("dir/", g)).into_bytes(),
            "slash",
        ),
        ("dot", format!("{}\n", line("a/./b", g)).into_bytes(), "`.`"),
        (
            "bidi",
            format!("{}\n", line("invoice\u{202E}fdp.exe", g)).into_bytes(),
            "bidirectional",
        ),
        (
            "reserved",
            format!("{}\n", line("errors.txt", g)).into_bytes(),
            "reserved",
        ),
        (
            "unknown blob",
            format!("{}\n", line("a", &unknown)).into_bytes(),
            "does not exist",
        ),
        (
            "not ready",
            format!("{}\n", line("a", id_of(&pending))).into_bytes(),
            "is created",
        ),
        (
            "infected",
            format!("{}\n", line("a", id_of(&infected))).into_bytes(),
            "is infected",
        ),
        (
            "deleted",
            format!("{}\n", line("a", id_of(&deleted))).into_bytes(),
            "is deleted",
        ),
    ];
    let created = create(&app, archive_blob("bad.zip")).await;
    let id = id_of(&created);
    let (status, body) = put_part(&app, id, 1, manifest(&[("first.png", g)])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for (name, bytes, message) in cases {
        let (status, body) = put_part(&app, id, 2, bytes).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {body}");
        let said = body["error"]["message"].as_str().unwrap();
        assert!(said.contains(message), "{name}: {body}");
        let got = get_blob(&app, id).await;
        assert_eq!(got["state"], "created", "{name}");
        assert_eq!(got["source_parts"], 1, "{name}");
        assert_eq!(got["source_rows"], 1, "{name}");
        assert_eq!(
            source_objects(&app, id).await,
            vec![format!("{id}/source/1")],
            "{name}"
        );
    }
    // The part that failed can be sent again.
    let (status, body) = put_part(&app, id, 2, manifest(&[("second.png", g)])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source_parts"], 2);
}

#[tokio::test]
async fn parts_are_consecutive_and_finish_needs_them() {
    let app = setup().await;
    let member = ready_blob(&app, &png_bytes(100)).await;
    let line = manifest(&[("a.png", id_of(&member))]);
    let created = create(&app, archive_blob("seq.zip")).await;
    let id = id_of(&created);

    let (status, body) = finish(&app, id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no parts")
    );

    let (status, body) = put_part(&app, id, 2, line.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("expected part 1")
    );
    let (status, _) = put_part(&app, id, 0, line.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        source_objects(&app, id).await.is_empty(),
        "refused before any byte"
    );

    let (status, _) = put_part(&app, id, 1, line.clone()).await;
    assert_eq!(status, StatusCode::OK);
    // A retry of a part that landed is harmless.
    let (status, body) = put_part(&app, id, 1, line.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("expected part 2")
    );
    // And so is `PUT /blobs/:id`, which would be part one again.
    let (status, _) = put_private(&app, id, line.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = put_part(&app, id, 10_001, line.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "out of sequence first");
    sqlx::query("UPDATE blobs SET source_parts = 10000 WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    let (status, body) = put_part(&app, id, 10_001, line.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    sqlx::query("UPDATE blobs SET source_parts = 1 WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();

    let (status, body) = finish(&app, id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    // Sealed: no more parts, no second finish, no file upload.
    let (status, body) = put_part(&app, id, 2, line.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("uploaded")
    );
    let (status, _) = finish(&app, id).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = put_private(&app, id, line).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(jobs_for(&app, id).await, vec!["archive"]);

    // Parts and finish do not apply to a file.
    let file = create(&app, blob_for(&png_bytes(100), "image/png", "raw")).await;
    let (status, body) = put_part(&app, id_of(&file), 1, png_bytes(100)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("raw"));
    let (status, _) = finish(&app, id_of(&file)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = finish(&app, &uuid::Uuid::now_v7().to_string()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Members are looked up in batches of a few hundred ids; a manifest longer
/// than one batch is checked to the end, and the dedupe counter runs on.
#[tokio::test]
async fn long_manifest_crosses_lookup_batches() {
    let app = setup_tuned(fake_clamd().await, |c| {
        c.upload.max_size = bytesize::ByteSize::kib(64);
    })
    .await;
    let member = ready_blob(&app, b"x").await;
    let good = id_of(&member).to_string();
    let n = 350;

    let created = create(&app, archive_blob("long.zip")).await;
    let id = id_of(&created);
    let mut lines: Vec<(&str, &str)> = vec![("a", good.as_str()); n];
    let unknown = uuid::Uuid::now_v7().to_string();
    lines[339] = ("a", unknown.as_str());
    let (status, body) = put_part(&app, id, 1, manifest(&lines)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("line 340:"),
        "{body}"
    );

    lines[339] = ("a", good.as_str());
    let (status, body) = put_private(&app, id, manifest(&lines)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source_rows"], n);
    assert!(run_job(&app, JobKind::Archive).await);
    let built = get_blob(&app, id).await;
    assert_eq!(built["archive_entries"], n);
    let entries = unzip(&download(&app, &built).await);
    assert_eq!(entries.len(), n);
    assert_eq!(entries[0].0, "a");
    assert_eq!(entries[1].0, "a-1");
    assert_eq!(entries[n - 1].0, format!("a-{}", n - 1));
}

#[tokio::test]
async fn archive_size_limit_is_checked_at_accept() {
    let app = setup_tuned(fake_clamd().await, |c| {
        c.archive.max_size = bytesize::ByteSize::kib(2);
    })
    .await;
    let member = ready_blob(&app, &png_bytes(900)).await;
    let m = id_of(&member);
    let created = create(&app, archive_blob("big.zip")).await;
    let id = id_of(&created);
    let (status, body) = put_part(&app, id, 1, manifest(&[("a.png", m)])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The second part pushes the estimate over the limit.
    let (status, body) = put_part(&app, id, 2, manifest(&[("b.png", m), ("c.png", m)])).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("archive.max_size")
    );
    assert_eq!(get_blob(&app, id).await["source_parts"], 1);
}

/// A member that was ready at accept and is gone at build time is skipped
/// and listed; the archive is still built.
#[tokio::test]
async fn members_gone_before_the_build_are_listed_in_errors_txt() {
    let app = setup().await;
    let kept = ready_blob(&app, &png_bytes(200)).await;
    let deleted = ready_blob(&app, &png_bytes(300)).await;
    let expired = ready_blob(&app, &png_bytes(400)).await;
    let created = create(&app, archive_blob("partial.zip")).await;
    let id = id_of(&created);
    let (status, body) = put_private(
        &app,
        id,
        manifest(&[
            ("gone/deleted.png", id_of(&deleted)),
            ("kept.png", id_of(&kept)),
            ("gone/expired.png", id_of(&expired)),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(delete(&app, id_of(&deleted)).await, StatusCode::NO_CONTENT);
    sqlx::query("UPDATE blobs SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(parse_id(id_of(&expired)))
        .execute(app.state.pool())
        .await
        .unwrap();

    assert!(run_job(&app, JobKind::Archive).await);
    let built = get_blob(&app, id).await;
    assert_eq!(built["state"], "ready", "{built}");
    assert_eq!(built["archive_entries"], 1);
    assert_eq!(built["archive_errors"], 2);
    let entries = unzip(&download(&app, &built).await);
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["kept.png", "ERRORS.txt"]);
    assert_eq!(entries[0].1, png_bytes(200));
    let errors = String::from_utf8(entries[1].1.clone()).unwrap();
    assert_eq!(
        errors,
        format!(
            "gone/deleted.png\t{}\tdeleted\ngone/expired.png\t{}\texpired\n",
            id_of(&deleted),
            id_of(&expired)
        )
    );
}

/// The blob is deleted while its archive is being built: nothing is
/// written and the job is done.
#[tokio::test]
async fn delete_before_the_build_writes_nothing() {
    let app = setup().await;
    let member = ready_blob(&app, &png_bytes(200)).await;
    let created = create(&app, archive_blob("gone.zip")).await;
    let id = id_of(&created);
    let (status, _) = put_private(&app, id, manifest(&[("a.png", id_of(&member))])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delete(&app, id).await, StatusCode::NO_CONTENT);
    assert!(run_job(&app, JobKind::Archive).await);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["state"], "deleted");
    assert!(blob["archive_entries"].is_null());
    let objects: Vec<String> = stored_objects(&app)
        .await
        .into_iter()
        .map(|o| o.location.to_string())
        .filter(|k| k.starts_with(id))
        .collect();
    assert_eq!(
        objects,
        vec![format!("{id}/source/1")],
        "parts wait for the purge"
    );
    assert_eq!(jobs_for(&app, id).await, vec!["purge"]);
}

/// A `created` blob holding parts has objects: a delete purges them, and
/// a source nobody finishes is expired by the reaper and purged.
#[tokio::test]
async fn sources_are_purged_when_deleted_or_abandoned() {
    let app = setup().await;
    let member = ready_blob(&app, &png_bytes(100)).await;
    let line = manifest(&[("a.png", id_of(&member))]);

    let deleted = create(&app, archive_blob("deleted.zip")).await;
    let (status, _) = put_part(&app, id_of(&deleted), 1, line.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delete(&app, id_of(&deleted)).await, StatusCode::NO_CONTENT);
    assert_eq!(jobs_for(&app, id_of(&deleted)).await, vec!["purge"]);

    let abandoned = create(&app, archive_blob("abandoned.zip")).await;
    let (status, _) = put_part(&app, id_of(&abandoned), 1, line.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let fresh = create(&app, archive_blob("fresh.zip")).await;
    let (status, _) = put_part(&app, id_of(&fresh), 1, line).await;
    assert_eq!(status, StatusCode::OK);
    let untouched = create(&app, archive_blob("untouched.zip")).await;

    sqlx::query("UPDATE blobs SET source_updated_at = now() - interval '2 days' WHERE id = $1")
        .bind(parse_id(id_of(&abandoned)))
        .execute(app.state.pool())
        .await
        .unwrap();
    let report = reap::reap(&app.state, Timestamp::now()).await.unwrap();
    assert_eq!(report.abandoned, 1);
    assert_eq!(report.purges, 1);
    assert_eq!(get_blob(&app, id_of(&abandoned)).await["state"], "expired");
    assert_eq!(get_blob(&app, id_of(&fresh)).await["state"], "created");
    assert_eq!(get_blob(&app, id_of(&untouched)).await["state"], "created");

    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'purge'")
        .execute(app.state.pool())
        .await
        .unwrap();
    while run_job(&app, JobKind::Purge).await {}
    assert!(source_objects(&app, id_of(&deleted)).await.is_empty());
    assert!(source_objects(&app, id_of(&abandoned)).await.is_empty());
    assert_eq!(source_objects(&app, id_of(&fresh)).await.len(), 1);
    assert!(get_blob(&app, id_of(&abandoned)).await["purged_at"].is_string());
}

/// A part whose body stops midway is a failed upload: the blob goes back
/// to `created` with the parts it had and nothing of the part remains.
#[tokio::test]
async fn dropped_part_is_aborted() {
    let app = setup().await;
    let member = ready_blob(&app, &png_bytes(100)).await;
    let created = create(&app, archive_blob("dropped.zip")).await;
    let id = id_of(&created);
    let line = manifest(&[("a.png", id_of(&member))]);

    let failing = futures::stream::iter([
        Ok(bytes::Bytes::from(line.clone())),
        Err(std::io::Error::other("connection reset by peer")),
    ]);
    let put = axum::http::Request::builder()
        .method(Method::PUT)
        .uri(format!("/blobs/{id}/parts/1"))
        .body(axum::body::Body::from_stream(failing))
        .unwrap();
    let (status, body) = call(&app.private, put).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "created");
    assert_eq!(got["source_parts"], 0);
    assert!(source_objects(&app, id).await.is_empty());

    let (status, _) = put_part(&app, id, 1, line).await;
    assert_eq!(status, StatusCode::OK);
}

/// A member larger than the look-ahead reads whole is opened when its turn
/// comes and streamed; small ones around it are prefetched. The archive
/// comes out the same either way.
#[tokio::test]
async fn large_members_are_streamed_when_reached() {
    let app = setup_tuned(fake_clamd().await, |c| {
        c.upload.max_size = bytesize::ByteSize::mib(10);
    })
    .await;
    let big_bytes = png_bytes(9 * 1024 * 1024);
    let big = ready_blob(&app, &big_bytes).await;
    let small = ready_blob(&app, &png_bytes(500)).await;
    let created = create(&app, archive_blob("mixed.zip")).await;
    let id = id_of(&created);
    let (status, body) = put_private(
        &app,
        id,
        manifest(&[
            ("a.png", id_of(&small)),
            ("big.png", id_of(&big)),
            ("c.png", id_of(&small)),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(run_job(&app, JobKind::Archive).await);
    let built = get_blob(&app, id).await;
    assert_eq!(built["state"], "ready", "{built}");
    assert_eq!(built["archive_entries"], 3);
    let entries = unzip(&download(&app, &built).await);
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["a.png", "big.png", "c.png"]);
    assert_eq!(entries[0].1, png_bytes(500));
    assert_eq!(entries[1].1, big_bytes);
    assert_eq!(entries[2].1, png_bytes(500));
}

/// A build that cannot succeed on a retry, here a row stripped of its key
/// material, is dead after its first attempt rather than retried through
/// the backoff; the blob stays `uploaded`.
#[tokio::test]
async fn permanent_build_failures_die_at_once() {
    let app = setup().await;
    let member = ready_blob(&app, &png_bytes(100)).await;
    let created = create(&app, archive_blob("broken.zip")).await;
    let id = id_of(&created);
    let (status, _) = put_private(&app, id, manifest(&[("a.png", id_of(&member))])).await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query("UPDATE blobs SET wrapped_key = NULL WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    assert!(run_job(&app, JobKind::Archive).await);
    let (attempts, dead, error): (i32, bool, String) = sqlx::query_as(
        "SELECT attempts, failed_at IS NOT NULL, last_error FROM jobs \
         WHERE kind = 'archive' AND blob_id = $1",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    assert_eq!((attempts, dead), (1, true), "{error}");
    assert!(error.contains("wrapped key"), "{error}");
    assert_eq!(get_blob(&app, id).await["state"], "uploaded");
}
