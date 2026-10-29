//! Generated documents: a typst workspace accepted as a manifest, in
//! parts, the finish, the build job compiling it in the sandbox into a
//! PDF, and the ways a workspace or a template fails.

use agrafe::jobs::JobKind;
use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::support::*;

fn document_blob(filename: &str) -> Value {
    json!({
        "filename": filename,
        "content_type": "application/pdf",
        "input_type": "document",
    })
}

/// A manifest line taking a file from a blob.
fn from_blob(path: &str, id: &str) -> Value {
    json!({"path": path, "blob_id": id})
}

/// A manifest line carrying a text file.
fn text(path: &str, text: &str) -> Value {
    json!({"path": path, "text": text})
}

/// A manifest of lines.
fn manifest(lines: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(line.to_string().as_bytes());
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
    assert_eq!(headers["content-type"], "application/pdf");
    bytes
}

/// The one job of `kind` for the blob, as `(attempts, dead, last_error)`.
async fn job_outcome(app: &TestApp, id: &str) -> (i32, bool, String) {
    sqlx::query_as(
        "SELECT attempts, failed_at IS NOT NULL, last_error FROM jobs \
         WHERE kind = 'document' AND blob_id = $1",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap()
}

/// A template using what a workspace offers: an include, data and an
/// image. Fonts come from the workspace the same way; the helper's own
/// tests cover their registration.
const TEMPLATE: &str = r#"#import "lib/header.typ": header
#set page(width: 12cm, height: 8cm, margin: 1cm)
#let d = json("data.json")
#header(d.titre)
#for ligne in d.lignes [- #ligne.libelle: #ligne.montant €]
#image("img/logo.png", width: 1cm)
"#;

const HEADER: &str = "#let header(t) = heading(level: 1, t)\n";

#[tokio::test]
async fn document_is_built_from_a_workspace_in_parts() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let template = ready_file(&app, "facture.typ", "text/plain", TEMPLATE.as_bytes()).await;
    let logo = ready_file(&app, "logo.png", "image/png", &fixture("photo.png")).await;

    let created = create(&app, document_blob("facture n° 12.pdf")).await;
    let id = id_of(&created);
    assert_eq!(created["state"], "created");
    assert_eq!(created["skip_virus_scan"], true, "generated");
    assert_eq!(created["scan_state"], "skipped");
    assert_eq!(created["public_upload_allowed"], false, "private-only");
    assert!(created["urls"]["upload"].is_null());
    assert_eq!(created["source_parts"], 0);
    assert!(created["size"].is_null());

    // The reusable files in one part, the document's own data in another.
    let (status, body) = put_part(
        &app,
        id,
        1,
        manifest(&[
            from_blob("main.typ", id_of(&template)),
            text("lib/header.typ", HEADER),
            from_blob("img/logo.png", id_of(&logo)),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "created");
    assert_eq!(body["source_parts"], 1);
    assert_eq!(body["source_rows"], 3);
    assert!(
        body["size"].is_null(),
        "a PDF's size is not known in advance"
    );
    let (status, body) = put_part(
        &app,
        id,
        2,
        manifest(&[json!({
            "path": "data.json",
            "json": {"titre": "Facture n° 12", "lignes": [
                {"libelle": "Conseil", "montant": "1 200,00"},
                {"libelle": "Déplacement", "montant": "80,00"},
            ]},
        })]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source_parts"], 2);
    assert_eq!(body["source_rows"], 4);
    assert!(
        jobs_for(&app, id).await.is_empty(),
        "nothing until the finish"
    );

    let (status, body) = finish(&app, id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert_eq!(jobs_for(&app, id).await, vec!["document"]);

    assert!(run_job(&app, JobKind::Document).await);
    let built = get_blob(&app, id).await;
    assert_eq!(built["state"], "ready", "{built}");
    assert!(built["archive_entries"].is_null());
    assert_eq!(jobs_for(&app, id).await, vec!["render"]);

    let bytes = download(&app, &built).await;
    // For checking the file with other tools: `AGRAFE_DOCUMENT_OUT=/tmp/x.pdf`.
    if let Ok(path) = std::env::var("AGRAFE_DOCUMENT_OUT") {
        std::fs::write(path, &bytes).unwrap();
    }
    assert!(
        bytes.starts_with(b"%PDF-"),
        "{:?}",
        &bytes[..8.min(bytes.len())]
    );
    assert_eq!(built["size"], bytes.len());
    assert_eq!(built["checksum"], STANDARD.encode(Sha256::digest(&bytes)));
    assert_eq!(
        storage_key(&app, id).await,
        object_key(id, "original"),
        "the source parts stay under the prefix until the purge"
    );

    // Once ready the PDF is rendered like any other: a page of text gets
    // its first page as a thumbnail and no preview.
    while run_job(&app, JobKind::Render).await {}
    let rendered = get_blob(&app, id).await;
    assert_eq!(rendered["thumbnail_state"], "ready", "{rendered}");
    assert_eq!(rendered["preview_state"], "unavailable");
}

/// `PUT /blobs/:id` is part one and the finish in one call, and `main`
/// names another entry file than `main.typ`.
#[tokio::test]
async fn whole_workspace_in_one_put_with_a_chosen_main() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let mut blob = document_blob("note.pdf");
    blob["main"] = json!("src/note.typ");
    let created = create(&app, blob).await;
    let id = id_of(&created);
    let (status, body) = put_private(
        &app,
        id,
        manifest(&[
            text(
                "src/note.typ",
                "#import \"util.typ\": shout\n#shout(\"bonjour\")",
            ),
            text("src/util.typ", "#let shout(s) = upper(s)"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert!(run_job(&app, JobKind::Document).await);
    let built = get_blob(&app, id).await;
    assert_eq!(built["state"], "ready", "{built}");
    assert!(download(&app, &built).await.starts_with(b"%PDF-"));
}

/// An entry file whose name starts with `-` is a file like any other,
/// not a flag of the helper's.
#[tokio::test]
async fn a_main_starting_with_a_dash_compiles() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let mut blob = document_blob("dash.pdf");
    blob["main"] = json!("-x.typ");
    let created = create(&app, blob).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, manifest(&[text("-x.typ", "Bonjour")])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(run_job(&app, JobKind::Document).await);
    let built = get_blob(&app, id).await;
    assert_eq!(built["state"], "ready", "{built}");
}

#[tokio::test]
async fn creation_rules() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "not a PDF",
            json!({"filename": "a.zip", "content_type": "application/zip", "input_type": "document"}),
            "cannot be produced",
        ),
        (
            "declared size",
            json!({"filename": "a.pdf", "content_type": "application/pdf", "input_type": "document", "size": 1}),
            "must be omitted",
        ),
        (
            "bad main",
            json!({"filename": "a.pdf", "content_type": "application/pdf", "input_type": "document", "main": "../x.typ"}),
            "main:",
        ),
        (
            "main on an archive",
            json!({"filename": "a.zip", "content_type": "application/zip", "input_type": "archive", "main": "main.typ"}),
            "document input only",
        ),
        (
            "columns on a document",
            json!({"filename": "a.pdf", "content_type": "application/pdf", "input_type": "document", "columns": [{"name": "a"}]}),
            "rows input only",
        ),
    ];
    for (name, blob, message) in cases {
        let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", blob)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {body}");
        let said = body["error"]["message"].as_str().unwrap();
        assert!(said.contains(message), "{name}: {body}");
    }
    // The public port never uploads a document: no upload URL is minted.
    let created = create(&app, document_blob("ok.pdf")).await;
    assert!(created["urls"]["upload"].is_null());
    let (status, _) = call(
        &app.private,
        json_req(
            Method::POST,
            &format!("/blobs/{}/urls", id_of(&created)),
            json!({"ttl": "1m"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// Every bad line answers 422 naming it, the part is discarded, the blob
/// is back in `created` with the parts it had.
#[tokio::test]
async fn manifest_rejections() {
    let db = TestDatabase::new().await;
    let app = setup_tuned(db, fake_clamd().await, |c| {
        c.document.max_workspace_size = bytesize::ByteSize::kib(2);
        c.document.max_files = 5;
    })
    .await;
    let good = ready_blob(&app, &png_bytes(500)).await;
    let g = id_of(&good).to_string();
    let g = g.as_str();
    let pending = create(&app, blob_for(&png_bytes(100), "image/png", "raw")).await;
    let deleted = ready_blob(&app, &png_bytes(100)).await;
    assert_eq!(delete(&app, id_of(&deleted)).await, StatusCode::NO_CONTENT);
    let unknown = uuid::Uuid::now_v7().to_string();

    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("not json", b"nope\n".to_vec(), "line 1"),
        ("not an object", b"[1]\n".to_vec(), "line 1"),
        ("no source", manifest(&[json!({"path": "a"})]), "needs one"),
        (
            "two sources",
            manifest(&[json!({"path": "a", "blob_id": g, "text": ""})]),
            "only one",
        ),
        (
            "unknown field",
            manifest(&[json!({"path": "a", "text": "", "x": 1})]),
            "line 1",
        ),
        (
            "text not a string",
            manifest(&[json!({"path": "a", "text": 1})]),
            "line 1",
        ),
        (
            "no trailing newline",
            text("a.typ", "").to_string().into_bytes(),
            "newline",
        ),
        (
            "second line bad",
            manifest(&[text("a.typ", ""), text("../b.typ", "")]),
            "line 2",
        ),
        ("absolute", manifest(&[text("/a", "")]), "relative"),
        ("backslash", manifest(&[text("a\\b", "")]), "backslash"),
        ("empty path", manifest(&[text("", "")]), "empty"),
        ("trailing slash", manifest(&[text("dir/", "")]), "slash"),
        ("dot", manifest(&[text("a/./b", "")]), "`.`"),
        (
            "bidi",
            manifest(&[text("invoice\u{202E}fdp.typ", "")]),
            "bidirectional",
        ),
        (
            "long component",
            manifest(&[text(&"a".repeat(256), "")]),
            "component longer",
        ),
        (
            "twice in the part",
            manifest(&[text("a.typ", ""), text("a.typ", "")]),
            "appears twice",
        ),
        (
            "unknown blob",
            manifest(&[from_blob("a", &unknown)]),
            "does not exist",
        ),
        (
            "not ready",
            manifest(&[from_blob("a", id_of(&pending))]),
            "is created",
        ),
        (
            "deleted",
            manifest(&[from_blob("a", id_of(&deleted))]),
            "is deleted",
        ),
        (
            "workspace too large by inline text",
            manifest(&[text("big.txt", &"x".repeat(1600))]),
            "document.max_workspace_size",
        ),
        (
            "workspace too large by members",
            manifest(&[
                from_blob("a.png", g),
                from_blob("b.png", g),
                from_blob("c.png", g),
                from_blob("d.png", g),
            ]),
            "document.max_workspace_size",
        ),
        (
            // Five with the one already accepted: bytes do not bound it.
            "too many files",
            manifest(
                &(0..5)
                    .map(|i| text(&format!("{i}"), ""))
                    .collect::<Vec<_>>(),
            ),
            "document.max_files",
        ),
    ];
    let created = create(&app, document_blob("bad.pdf")).await;
    let id = id_of(&created);
    let (status, body) = put_part(&app, id, 1, manifest(&[from_blob("first.png", g)])).await;
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
    }
    // The same path in another part is the build's to refuse, not accept's.
    let (status, body) = put_part(&app, id, 2, manifest(&[text("first.png", "")])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = finish(&app, id).await;
    assert_eq!(status, StatusCode::OK);
    assert!(run_job(&app, JobKind::Document).await);
    let (attempts, dead, error) = job_outcome(&app, id).await;
    assert_eq!((attempts, dead), (1, true), "{error}");
    assert!(error.contains("part 2, line 1"), "{error}");
    assert!(error.contains("appears twice"), "{error}");
    assert_eq!(get_blob(&app, id).await["state"], "uploaded");
}

/// A template that does not compile kills the job at once, with the
/// compiler's diagnostics, located in the workspace, as the error.
#[tokio::test]
async fn compile_errors_die_at_once_with_diagnostics() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, document_blob("broken.pdf")).await;
    let id = id_of(&created);
    let (status, _) = put_private(
        &app,
        id,
        manifest(&[
            text("main.typ", "= Titre\n#include \"lib/part.typ\"\n"),
            text("lib/part.typ", "#let x = 1\n#nope(x)\n"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(run_job(&app, JobKind::Document).await);
    let (attempts, dead, error) = job_outcome(&app, id).await;
    assert_eq!((attempts, dead), (1, true), "{error}");
    assert!(error.contains("does not compile"), "{error}");
    assert!(
        error.contains("lib/part.typ:2:2: error: unknown variable: nope"),
        "{error}"
    );
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["state"], "uploaded");
    assert!(blob["failed_at"].is_string());
    assert_eq!(blob["last_error"], error);
}

/// A helper that fails on its own account, not with the compiler's exit
/// code, is a failed attempt, not a dead document: the same template may
/// well compile once the disk or the helper is back.
#[tokio::test]
async fn a_helper_failing_for_its_own_reasons_is_retried() {
    let db = TestDatabase::new().await;
    let app = setup_tuned(db, fake_clamd().await, |c| {
        c.media.render.render_document_command = "/usr/bin/false".into();
    })
    .await;
    let created = create(&app, document_blob("unlucky.pdf")).await;
    let id = id_of(&created);
    let (status, _) = put_private(&app, id, manifest(&[text("main.typ", "= Hi\n")])).await;
    assert_eq!(status, StatusCode::OK);
    assert!(run_job(&app, JobKind::Document).await);
    let (attempts, dead, error) = job_outcome(&app, id).await;
    assert_eq!((attempts, dead), (1, false), "{error}");
    assert!(error.contains("compiler helper failed"), "{error}");
    assert_eq!(get_blob(&app, id).await["state"], "uploaded");
}

/// A template that outruns `media.sandbox.timeout` is a template that
/// does not compile: no retry would let it finish.
#[cfg(unix)]
#[tokio::test]
async fn a_build_that_times_out_dies_at_once() {
    use std::os::unix::fs::PermissionsExt;
    let db = TestDatabase::new().await;
    let slow = std::env::temp_dir().join(format!("agrafe-slow-document-{}", uuid::Uuid::now_v7()));
    std::fs::write(&slow, "#!/bin/sh\nsleep 5\n").unwrap();
    std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    let app = setup_tuned(db, fake_clamd().await, |c| {
        c.media.render.render_document_command = slow.to_string_lossy().into_owned();
        c.media.render.sandbox.timeout = std::time::Duration::from_millis(300);
    })
    .await;
    let created = create(&app, document_blob("slow.pdf")).await;
    let id = id_of(&created);
    let (status, _) = put_private(&app, id, manifest(&[text("main.typ", "= Hi\n")])).await;
    assert_eq!(status, StatusCode::OK);
    assert!(run_job(&app, JobKind::Document).await);
    std::fs::remove_file(&slow).unwrap();
    let (attempts, dead, error) = job_outcome(&app, id).await;
    assert_eq!((attempts, dead), (1, true), "{error}");
    assert!(error.contains("does not compile"), "{error}");
    assert!(error.contains("exceeded the time limit"), "{error}");
}

/// A helper that exits cleanly without writing the PDF is not the
/// compiler: a failed attempt, and the file it did not write named.
#[tokio::test]
async fn a_helper_writing_nothing_is_a_failed_attempt() {
    let db = TestDatabase::new().await;
    let app = setup_tuned(db, fake_clamd().await, |c| {
        c.media.render.render_document_command = "/usr/bin/true".into();
    })
    .await;
    let created = create(&app, document_blob("empty.pdf")).await;
    let id = id_of(&created);
    let (status, _) = put_private(&app, id, manifest(&[text("main.typ", "= Hi\n")])).await;
    assert_eq!(status, StatusCode::OK);
    assert!(run_job(&app, JobKind::Document).await);
    let (attempts, dead, error) = job_outcome(&app, id).await;
    assert_eq!((attempts, dead), (1, false), "{error}");
    assert!(error.contains("without writing the PDF"), "{error}");
    assert_eq!(get_blob(&app, id).await["state"], "uploaded");
}

/// A part longer than `upload.max_size` is refused as it streams in.
#[tokio::test]
async fn a_part_over_the_maximum_size_is_too_large() {
    let db = TestDatabase::new().await;
    let app = setup_tuned(db, fake_clamd().await, |c| {
        c.upload.max_size = bytesize::ByteSize::kib(1);
    })
    .await;
    let created = create(&app, document_blob("big.pdf")).await;
    let id = id_of(&created);
    let (status, body) = put_part(
        &app,
        id,
        1,
        manifest(&[text("main.typ", &"x".repeat(1100))]),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(
        body["error"]["message"],
        "part exceeds the maximum of 1024 bytes"
    );
    assert_eq!(get_blob(&app, id).await["source_parts"], 0);
}

/// What a template may not do: fetch a package, read outside the
/// workspace, or need a file the manifest does not hold.
#[tokio::test]
async fn templates_are_confined_to_the_workspace() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let cases: Vec<(&str, &str, &str)> = vec![
        (
            "package",
            "#import \"@preview/cetz:0.3.0\": canvas\n",
            "packages are not available",
        ),
        (
            "escape",
            "#include \"../../etc/passwd\"\n",
            "would escape the project root",
        ),
        (
            "escape through the root",
            "#image(\"/../../etc/passwd\")\n",
            "would escape the project root",
        ),
        (
            "missing include",
            "#include \"lib/none.typ\"\n",
            "lib/none.typ",
        ),
        ("missing data", "#json(\"data.json\")\n", "data.json"),
    ];
    for (name, template, message) in cases {
        let created = create(&app, document_blob(&format!("{name}.pdf"))).await;
        let id = id_of(&created);
        let (status, _) = put_private(&app, id, manifest(&[text("main.typ", template)])).await;
        assert_eq!(status, StatusCode::OK, "{name}");
        assert!(run_job(&app, JobKind::Document).await);
        let (attempts, dead, error) = job_outcome(&app, id).await;
        assert_eq!((attempts, dead), (1, true), "{name}: {error}");
        assert!(error.contains(message), "{name}: {error}");
    }
    // No main file at all: the manifest named another.
    let created = create(&app, document_blob("nomain.pdf")).await;
    let id = id_of(&created);
    let (status, _) = put_private(&app, id, manifest(&[text("other.typ", "x")])).await;
    assert_eq!(status, StatusCode::OK);
    assert!(run_job(&app, JobKind::Document).await);
    let (_, dead, error) = job_outcome(&app, id).await;
    assert!(dead, "{error}");
    assert!(error.contains("main.typ"), "{error}");
}

/// A workspace file whose blob was ready at accept and is gone at build
/// time kills the build: a document cannot be compiled without it.
#[tokio::test]
async fn a_member_gone_before_the_build_kills_it() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let logo = ready_blob(&app, &fixture("photo.png")).await;
    let created = create(&app, document_blob("gone.pdf")).await;
    let id = id_of(&created);
    let (status, _) = put_private(
        &app,
        id,
        manifest(&[
            text("main.typ", "#image(\"logo.png\")"),
            from_blob("logo.png", id_of(&logo)),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delete(&app, id_of(&logo)).await, StatusCode::NO_CONTENT);
    assert!(run_job(&app, JobKind::Document).await);
    let (attempts, dead, error) = job_outcome(&app, id).await;
    assert_eq!((attempts, dead), (1, true), "{error}");
    assert!(error.contains("workspace file logo.png"), "{error}");
    assert!(error.contains("is deleted"), "{error}");
    assert_eq!(get_blob(&app, id).await["state"], "uploaded");
}

/// A path that is a file on one line and a directory on another cannot
/// be written out; the build refuses it at the line.
#[tokio::test]
async fn a_file_and_a_directory_cannot_share_a_path() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, document_blob("clash.pdf")).await;
    let id = id_of(&created);
    let (status, _) = put_part(
        &app,
        id,
        1,
        manifest(&[text("main.typ", "x"), text("lib", "")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = put_part(&app, id, 2, manifest(&[text("lib/a.typ", "")])).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = finish(&app, id).await;
    assert_eq!(status, StatusCode::OK);
    assert!(run_job(&app, JobKind::Document).await);
    let (_, dead, error) = job_outcome(&app, id).await;
    assert!(dead, "{error}");
    assert!(error.contains("part 2, line 1"), "{error}");
    assert!(error.contains("both a file and a directory"), "{error}");
}

/// `document.max_files` bounds the directories the paths make as it
/// bounds the files, and a path is at most 32 components deep: a few deep,
/// distinct paths would otherwise cost a directory per component.
#[tokio::test]
async fn the_directories_a_workspace_makes_are_bounded() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| c.document.max_files = 5).await;
    let created = create(&app, document_blob("deep.pdf")).await;
    let id = id_of(&created);

    let deep = format!("{}/f.typ", ["d"; 32].join("/"));
    let (status, body) = put_part(&app, id, 1, manifest(&[text(&deep, "")])).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("32 components deep"),
        "{body}"
    );

    // Three files, six directories.
    let lines = [
        text("main.typ", "x"),
        text("a/b/c/f.typ", ""),
        text("x/y/z/f.typ", ""),
    ];
    let (status, body) = put_part(&app, id, 1, manifest(&lines)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = finish(&app, id).await;
    assert_eq!(status, StatusCode::OK);
    assert!(run_job(&app, JobKind::Document).await);
    let (_, dead, error) = job_outcome(&app, id).await;
    assert!(dead, "{error}");
    assert!(
        error.contains("maximum of 5 directories (document.max_files)"),
        "{error}"
    );
}

/// The blob is deleted while its document waits to be built: nothing is
/// written and the job is done.
#[tokio::test]
async fn delete_before_the_build_writes_nothing() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, document_blob("deleted.pdf")).await;
    let id = id_of(&created);
    let (status, _) = put_private(&app, id, manifest(&[text("main.typ", "x")])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delete(&app, id).await, StatusCode::NO_CONTENT);
    assert!(run_job(&app, JobKind::Document).await);
    assert_eq!(get_blob(&app, id).await["state"], "deleted");
    let objects: Vec<String> = stored_objects(&app)
        .await
        .into_iter()
        .map(|o| o.location.to_string())
        .filter(|k| k.starts_with(&object_key(id, "")))
        .collect();
    assert_eq!(objects, vec![object_key(id, "source/1")]);
    assert_eq!(jobs_for(&app, id).await, vec!["purge"]);
}

/// Workers that build documents do not start on a helper that fails its
/// probe: the real one compiles the probe workspace, a wrong binary or an
/// idle one does not.
#[tokio::test]
async fn the_startup_probe_compiles_a_workspace() {
    let db = TestDatabase::new().await;
    let app = setup(db.clone()).await;
    let media = &app.state.config().media;
    let probe = |render| agrafe_render::probe_document(render, media.tmp_dir.as_deref());
    probe(&media.render).await.unwrap();

    let wrong_binary = agrafe_render::Config {
        render_document_command: "/usr/bin/false".into(),
        ..media.render.clone()
    };
    let err = probe(&wrong_binary).await.unwrap_err().to_string();
    assert!(err.contains("failed on the probe template"), "{err}");
    assert!(err.contains("media.render_document_command"), "{err}");

    let idle_binary = agrafe_render::Config {
        render_document_command: "/usr/bin/true".into(),
        ..media.render.clone()
    };
    let err = probe(&idle_binary).await.unwrap_err().to_string();
    assert!(err.contains("wrote nothing"), "{err}");

    let app = setup_tuned(db, fake_clamd().await, |c| {
        c.jobs.kinds = Some(vec![JobKind::Document]);
        c.media.render.render_document_command = "/nonexistent/agrafe-render-document".into();
    })
    .await;
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agrafe::jobs::runner::run(app.state.clone(), std::future::pending::<()>()),
    )
    .await
    .expect("the workers give up at once")
    .unwrap_err();
    assert!(err.to_string().contains("startup probe"), "{err}");
}
