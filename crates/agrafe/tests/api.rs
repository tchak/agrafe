//! Integration tests against the real routers and a real Postgres database.
//!
//! `DATABASE_URL` selects the database (default `postgres://localhost/agrafe_test`).
//! Tests share it and serialise through a mutex.

use agrafe::api;
use agrafe::app::AppState;
use agrafe::config::Config;
use agrafe::db;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, MutexGuard};
use tower::ServiceExt;

static LOCK: Mutex<()> = Mutex::const_new(());

struct TestApp {
    private: Router,
    public: Router,
    state: AppState,
    _guard: MutexGuard<'static, ()>,
}

async fn setup() -> TestApp {
    let guard = LOCK.lock().await;
    let db_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        let user = std::env::var("USER").unwrap_or_else(|_| "postgres".to_string());
        format!("postgres://{user}@localhost/agrafe_test")
    });
    // Tests share the storage directory; start each one empty.
    let storage = std::env::temp_dir().join("agrafe-test-storage");
    let _ = std::fs::remove_dir_all(&storage);
    let key = STANDARD.encode([42u8; 32]);
    let toml = format!(
        r#"
        [server]
        private_addr = "127.0.0.1:0"
        public_addr = "127.0.0.1:0"
        public_base_url = "https://agrafe.test"
        [database]
        url = "{db_url}"
        max_connections = 2
        [storage]
        backend = "local"
        [storage.local]
        path = "{tmp}"
        [signing]
        current_key = "k1"
        window = "1h"
        [signing.keys]
        k1 = "{key}"
        [signing.ttl]
        upload = "1h"
        metadata = "2h"
        read = "24h"
        [encryption]
        current_kek = "kek1"
        chunk_size = "1 KiB"
        [encryption.keks]
        kek1 = "{kek}"
        "#,
        kek = STANDARD.encode([43u8; 32]),
        tmp = storage.display(),
    );
    let dir = tempdir();
    let path = dir.join("agrafe.toml");
    std::fs::write(&path, toml).unwrap();
    let config = Config::load(Some(&path)).unwrap();

    let pool = db::connect(&config.database).await.unwrap();
    db::migrate(&pool).await.unwrap();
    sqlx::query("TRUNCATE jobs, blobs")
        .execute(&pool)
        .await
        .unwrap();

    let state = AppState::new(config, pool).unwrap();
    TestApp {
        private: api::private_router(state.clone()),
        public: api::public_router(state.clone()),
        state,
        _guard: guard,
    }
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("agrafe-test-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn call(router: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, json)
}

fn json_req(method: Method, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn req(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn raw_blob() -> Value {
    json!({
        "filename": "photo été.jpg",
        "content_type": "image/jpeg",
        "input_type": "raw",
        "checksum": STANDARD.encode([1u8; 32]),
        "size": 1024
    })
}

/// Strip the base so the URL can be sent to the public router.
fn public_path(url: &str) -> String {
    url.strip_prefix("https://agrafe.test").unwrap().to_string()
}

#[tokio::test]
async fn create_and_get_blob() {
    let app = setup().await;
    let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", raw_blob())).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "created");
    assert_eq!(body["category"], "image");
    assert_eq!(body["scan_state"], "pending");
    assert_eq!(body["public_upload_allowed"], true);
    assert_eq!(body["write_once"], true);
    assert!(body["urls"]["upload"].is_string());
    assert!(
        body["urls"]["original"]
            .as_str()
            .unwrap()
            .ends_with("/original/photo%20%C3%A9t%C3%A9.jpg")
    );

    let id = body["id"].as_str().unwrap();
    let (status, got) = call(&app.private, req(Method::GET, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["id"], body["id"]);
    assert_eq!(got["checksum"], STANDARD.encode([1u8; 32]));

    let (status, _) = call(
        &app.private,
        req(Method::GET, &format!("/blobs/{}", uuid::Uuid::now_v7())),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn validation_errors() {
    let app = setup().await;
    let mut no_checksum = raw_blob();
    no_checksum.as_object_mut().unwrap().remove("checksum");
    let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", no_checksum)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "unprocessable");

    let (status, _) = call(
        &app.private,
        json_req(
            Method::POST,
            "/blobs",
            json!({"filename": "x", "content_type": "bad", "input_type": "raw"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, _) = call(
        &app.private,
        json_req(
            Method::POST,
            "/blobs",
            json!({"filename": "export.csv", "content_type": "text/csv", "input_type": "rows", "size": 10}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, body) = call(
        &app.private,
        json_req(
            Method::POST,
            "/blobs",
            json!({"filename": "export.csv", "content_type": "text/csv", "input_type": "rows"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["category"], "spreadsheet");
    assert_eq!(body["public_upload_allowed"], false);
    assert!(body["urls"]["upload"].is_null());
}

#[tokio::test]
async fn private_only_blobs_get_no_public_upload() {
    let app = setup().await;
    for (field, value) in [
        ("write_once", json!(false)),
        ("skip_virus_scan", json!(true)),
    ] {
        let mut b = raw_blob();
        b[field] = value;
        let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", b)).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["public_upload_allowed"], false, "{field}");
        assert!(body["urls"]["upload"].is_null(), "{field}");
        if field == "skip_virus_scan" {
            assert_eq!(body["scan_state"], "skipped");
        }

        // A forged upload URL for such a blob is refused by the handler too.
        let id = body["id"].as_str().unwrap();
        let token = app
            .state
            .signer()
            .sign(&Method::PUT, 4_000_000_000, &format!("/{id}"));
        let (status, body) = call(
            &app.public,
            req(Method::PUT, &format!("/blobs/{token}/{id}")),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
}

#[tokio::test]
async fn public_signed_metadata() {
    let app = setup().await;
    let (_, created) = call(&app.private, json_req(Method::POST, "/blobs", raw_blob())).await;
    let metadata_url = public_path(created["urls"]["metadata"].as_str().unwrap());

    let (status, body) = call(&app.public, req(Method::GET, &metadata_url)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "created");
    assert!(
        body.get("filename").is_none(),
        "public metadata must not leak filenames"
    );

    // HEAD is accepted with a GET signature.
    let (status, _) = call(&app.public, req(Method::HEAD, &metadata_url)).await;
    assert_eq!(status, StatusCode::OK);

    // Tampered path: 403.
    let other = uuid::Uuid::now_v7();
    let tampered = format!("{}/{other}", metadata_url.rsplit_once('/').unwrap().0);
    let (status, body) = call(&app.public, req(Method::GET, &tampered)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Added query parameter: 403.
    let (status, _) = call(
        &app.public,
        req(Method::GET, &format!("{metadata_url}?x=1")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Wrong method for the signature: 403.
    let (status, _) = call(&app.public, req(Method::PUT, &metadata_url)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // No signature at all: the route does not even exist on the public port.
    let id = created["id"].as_str().unwrap();
    let (status, _) = call(&app.public, req(Method::GET, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Expired: 410.
    let expired = app.state.signer().sign(&Method::GET, 1, &format!("/{id}"));
    let (status, _) = call(
        &app.public,
        req(Method::GET, &format!("/blobs/{expired}/{id}")),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);

    // Valid signature on a missing blob: 404.
    let tok = app
        .state
        .signer()
        .sign(&Method::GET, 4_000_000_000, &format!("/{other}"));
    let (status, _) = call(
        &app.public,
        req(Method::GET, &format!("/blobs/{tok}/{other}")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn terminal_blobs_are_404_on_the_public_port() {
    let app = setup().await;
    for state in ["expired", "infected"] {
        let (_, created) = call(&app.private, json_req(Method::POST, "/blobs", raw_blob())).await;
        let id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();
        sqlx::query("UPDATE blobs SET state = $2 WHERE id = $1")
            .bind(id)
            .bind(state)
            .execute(app.state.pool())
            .await
            .unwrap();

        let metadata_url = public_path(created["urls"]["metadata"].as_str().unwrap());
        let (status, body) = call(&app.public, req(Method::GET, &metadata_url)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{state}: {body}");
        let upload_url = public_path(created["urls"]["upload"].as_str().unwrap());
        let (status, _) = call(&app.public, req(Method::PUT, &upload_url)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{state}");

        // The private port still reports the real state.
        let (status, body) = call(&app.private, req(Method::GET, &format!("/blobs/{id}"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], state);
    }
}

/// A PNG header followed by filler, long enough to span several 1 KiB
/// encryption segments.
fn png_bytes(len: usize) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    v.extend((v.len()..len).map(|i| (i * 31 % 251) as u8));
    v
}

/// A blob declaration matching `bytes`.
fn blob_for(bytes: &[u8], content_type: &str, input_type: &str) -> Value {
    json!({
        "filename": "photo.png",
        "content_type": content_type,
        "input_type": input_type,
        "checksum": STANDARD.encode(Sha256::digest(bytes)),
        "size": bytes.len()
    })
}

async fn create(app: &TestApp, blob: Value) -> Value {
    let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", blob)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

fn put_raw(uri: &str, bytes: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method(Method::PUT)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(bytes))
        .unwrap()
}

fn put_multipart(uri: &str, parts: &[(&str, &[u8])]) -> Request<Body> {
    let boundary = "agrafe-test-boundary";
    let mut body = Vec::new();
    for (name, bytes) in parts {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"x.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Request::builder()
        .method(Method::PUT)
        .uri(uri)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

/// Pending jobs for a blob, by kind.
async fn jobs_for(app: &TestApp, id: &str) -> Vec<String> {
    let id: uuid::Uuid = id.parse().unwrap();
    sqlx::query_scalar("SELECT kind FROM jobs WHERE blob_id = $1 ORDER BY kind")
        .bind(id)
        .fetch_all(app.state.pool())
        .await
        .unwrap()
}

/// Read and decrypt the stored original straight from the object store.
async fn stored_plaintext(app: &TestApp, id: &str) -> Vec<u8> {
    use agrafe::blob::repo;
    use agrafe_crypto::streaming::{self, Layout};
    use object_store::ObjectStoreExt;

    let blob = repo::find(app.state.pool(), id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    let key = blob.storage_key.as_deref().expect("storage_key");
    let bytes = app
        .state
        .store()
        .get(&object_store::path::Path::from(key))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let layout = Layout::new(blob.chunk_size.unwrap() as usize).unwrap();
    assert_eq!(
        bytes.len() as u64,
        layout.ciphertext_len(blob.size.unwrap() as u64)
    );
    assert_eq!(
        &bytes[..streaming::HEADER_LEN],
        blob.encryption_header.as_deref().unwrap()
    );
    let data_key = app
        .state
        .keks()
        .unwrap(
            blob.id,
            blob.kek_id.as_deref().unwrap(),
            blob.wrapped_key.as_deref().unwrap(),
        )
        .unwrap();
    streaming::decrypt(&data_key, key.as_bytes(), layout, &bytes).unwrap()
}

#[tokio::test]
async fn raw_upload_private() {
    let app = setup().await;
    let bytes = png_bytes(5000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = created["id"].as_str().unwrap();

    let (status, body) = call(
        &app.private,
        put_raw(&format!("/blobs/{id}"), bytes.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The scan is deferred, so the blob waits for it.
    assert_eq!(body["state"], "uploaded");
    assert_eq!(body["scan_state"], "pending");
    assert!(body["uploaded_at"].is_string());
    assert_eq!(jobs_for(&app, id).await, vec!["scan"]);
    assert_eq!(stored_plaintext(&app, id).await, bytes);

    // Write-once: a second upload is refused.
    let (status, _) = call(
        &app.private,
        put_raw(&format!("/blobs/{id}"), bytes.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Backend vouches for the source: ready at once, representations queued.
    let mut vouched = blob_for(&bytes, "image/png", "raw");
    vouched["skip_virus_scan"] = json!(true);
    let created = create(&app, vouched).await;
    let id = created["id"].as_str().unwrap();
    let (status, body) = call(
        &app.private,
        put_raw(&format!("/blobs/{id}"), bytes.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "ready");
    assert_eq!(body["scan_state"], "skipped");
    assert_eq!(jobs_for(&app, id).await, vec!["preview", "thumbnail"]);

    // An empty file is a valid file.
    let created = create(&app, blob_for(b"", "text/plain", "raw")).await;
    let id = created["id"].as_str().unwrap();
    let (status, body) = call(&app.private, put_raw(&format!("/blobs/{id}"), vec![])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(stored_plaintext(&app, id).await, b"");
}

#[tokio::test]
async fn raw_upload_rejections() {
    let app = setup().await;
    let bytes = png_bytes(3000);

    // Each failure answers 422, resets the blob to `created` and stores nothing.
    let cases: Vec<(&str, Value, Vec<u8>)> = vec![
        (
            "checksum",
            blob_for(&png_bytes(3001), "image/png", "raw"),
            {
                let mut b = bytes.clone();
                b.pop();
                b.push(0xff);
                b
            },
        ),
        (
            "too short",
            blob_for(&bytes, "image/png", "raw"),
            bytes[..2999].to_vec(),
        ),
        ("too long", blob_for(&bytes, "image/png", "raw"), {
            let mut b = bytes.clone();
            b.push(0);
            b
        }),
        ("type", blob_for(&bytes, "image/jpeg", "raw"), bytes.clone()),
    ];
    for (name, blob, body) in cases {
        let created = create(&app, blob).await;
        let id = created["id"].as_str().unwrap();
        let (status, res) = call(&app.private, put_raw(&format!("/blobs/{id}"), body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {res}");
        let (_, got) = call(&app.private, req(Method::GET, &format!("/blobs/{id}"))).await;
        assert_eq!(got["state"], "created", "{name}");
        assert!(got["uploaded_at"].is_null(), "{name}");
        assert!(jobs_for(&app, id).await.is_empty(), "{name}");
        // Can be retried.
        let (status, res) = call(
            &app.private,
            put_raw(&format!("/blobs/{id}"), bytes.clone()),
        )
        .await;
        if name == "type" || name == "checksum" {
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {res}");
        } else {
            assert_eq!(status, StatusCode::OK, "{name}: {res}");
        }
    }

    // Leftover objects: only the successful uploads wrote one.
    let objects: Vec<_> = {
        use futures::TryStreamExt;
        app.state
            .store()
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
    };
    assert_eq!(objects.len(), 2, "{objects:?}");

    // Generated input types are not uploadable yet.
    let created = create(
        &app,
        json!({"filename": "export.csv", "content_type": "text/csv", "input_type": "rows"}),
    )
    .await;
    let id = created["id"].as_str().unwrap();
    let (status, _) = call(
        &app.private,
        put_raw(&format!("/blobs/{id}"), b"a,b\n".to_vec()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn multipart_upload_public() {
    let app = setup().await;
    let bytes = png_bytes(2500);
    let created = create(&app, blob_for(&bytes, "image/png", "multipart")).await;
    let id = created["id"].as_str().unwrap();
    let upload = public_path(created["urls"]["upload"].as_str().unwrap());

    // Extra parts are ignored; the one named `file` is the file.
    let (status, body) = call(
        &app.public,
        put_multipart(
            &upload,
            &[("comment", b"hello"), ("file", &bytes), ("after", b"x")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert!(body.get("filename").is_none(), "public response is reduced");
    assert_eq!(stored_plaintext(&app, id).await, bytes);

    // No `file` part: 422 and the blob is back to `created`.
    let created = create(&app, blob_for(&bytes, "image/png", "multipart")).await;
    let id = created["id"].as_str().unwrap();
    let upload = public_path(created["urls"]["upload"].as_str().unwrap());
    let (status, body) = call(&app.public, put_multipart(&upload, &[("other", &bytes)])).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (_, got) = call(&app.private, req(Method::GET, &format!("/blobs/{id}"))).await;
    assert_eq!(got["state"], "created");

    // Not a multipart body at all.
    let (status, body) = call(&app.public, put_raw(&upload, bytes.clone())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // A raw body on the public port works too.
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let upload = public_path(created["urls"]["upload"].as_str().unwrap());
    let (status, body) = call(&app.public, put_raw(&upload, bytes.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn mutable_reupload() {
    let app = setup().await;
    let v1 = png_bytes(1500);
    let v2 = png_bytes(4000);
    let mut blob = blob_for(&v1, "image/png", "raw");
    blob["write_once"] = json!(false);
    blob["skip_virus_scan"] = json!(true);
    let created = create(&app, blob).await;
    let id = created["id"].as_str().unwrap();
    let uri = format!("/blobs/{id}");

    // Headers on a first upload must agree with the declaration.
    let wrong = Request::builder()
        .method(Method::PUT)
        .uri(&uri)
        .header("x-agrafe-checksum", STANDARD.encode(Sha256::digest(&v2)))
        .header("x-agrafe-size", v2.len())
        .body(Body::from(v1.clone()))
        .unwrap();
    let (status, body) = call(&app.private, wrong).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = call(&app.private, put_raw(&uri, v1.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "ready");
    let (_, first) = call(&app.private, req(Method::GET, &uri)).await;

    // A re-upload must say what is coming.
    let (status, body) = call(&app.private, put_raw(&uri, v2.clone())).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (_, got) = call(&app.private, req(Method::GET, &uri)).await;
    assert_eq!(got["state"], "ready", "previous version still served");

    let only_one = Request::builder()
        .method(Method::PUT)
        .uri(&uri)
        .header("x-agrafe-size", v2.len())
        .body(Body::from(v2.clone()))
        .unwrap();
    let (status, _) = call(&app.private, only_one).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let reupload = Request::builder()
        .method(Method::PUT)
        .uri(&uri)
        .header("x-agrafe-checksum", STANDARD.encode(Sha256::digest(&v2)))
        .header("x-agrafe-size", v2.len())
        .body(Body::from(v2.clone()))
        .unwrap();
    let (status, body) = call(&app.private, reupload).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "ready");
    assert_eq!(body["size"], v2.len());
    assert_eq!(body["checksum"], STANDARD.encode(Sha256::digest(&v2)));
    assert_eq!(stored_plaintext(&app, id).await, v2);

    // The previous object is queued for purge.
    let (kind, payload): (String, Value) =
        sqlx::query_as("SELECT kind, payload FROM jobs WHERE kind = 'purge'")
            .fetch_one(app.state.pool())
            .await
            .unwrap();
    assert_eq!(kind, "purge");
    assert_eq!(payload["blob_id"], first["id"]);
    assert!(
        payload["storage_key"]
            .as_str()
            .unwrap()
            .ends_with("/1/original"),
        "{payload}"
    );

    // The public port never accepts a re-upload, even with headers.
    assert!(created["urls"]["upload"].is_null());
}

#[tokio::test]
async fn mint_urls_with_options() {
    let app = setup().await;
    let (_, created) = call(&app.private, json_req(Method::POST, "/blobs", raw_blob())).await;
    let id = created["id"].as_str().unwrap();

    let (status, body) = call(
        &app.private,
        json_req(
            Method::POST,
            &format!("/blobs/{id}/urls"),
            json!({"ttl": "10m", "query": {"disposition": "attachment"}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let original = body["original"].as_str().unwrap();
    assert!(original.ends_with("?disposition=attachment"), "{original}");
    // Representation routes exist and accept the signed query string.
    let (status, _) = call(&app.public, req(Method::GET, &public_path(original))).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);

    let (status, body) = call(
        &app.private,
        json_req(
            Method::POST,
            &format!("/blobs/{id}/urls"),
            json!({"permanent": true}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Minting is stable inside a window.
    let (_, a) = call(
        &app.private,
        req(Method::POST, &format!("/blobs/{id}/urls")),
    )
    .await;
    let (_, b) = call(
        &app.private,
        req(Method::POST, &format!("/blobs/{id}/urls")),
    )
    .await;
    assert_eq!(a, b);
}

#[tokio::test]
async fn list_filters_and_pagination() {
    let app = setup().await;
    for i in 0..5 {
        let mut b = raw_blob();
        b["filename"] = json!(format!("f{i}.jpg"));
        if i % 2 == 0 {
            b["content_type"] = json!("application/pdf");
        }
        let (status, _) = call(&app.private, json_req(Method::POST, "/blobs", b)).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let (status, page1) = call(&app.private, req(Method::GET, "/blobs?limit=2")).await;
    assert_eq!(status, StatusCode::OK, "{page1}");
    assert_eq!(page1["items"].as_array().unwrap().len(), 2);
    assert_eq!(page1["items"][0]["filename"], "f4.jpg", "newest first");
    let cursor = page1["next_cursor"].as_str().unwrap();

    let (_, page2) = call(
        &app.private,
        req(Method::GET, &format!("/blobs?limit=2&cursor={cursor}")),
    )
    .await;
    assert_eq!(page2["items"][0]["filename"], "f2.jpg");
    let cursor = page2["next_cursor"].as_str().unwrap();
    let (_, page3) = call(
        &app.private,
        req(Method::GET, &format!("/blobs?limit=2&cursor={cursor}")),
    )
    .await;
    assert_eq!(page3["items"].as_array().unwrap().len(), 1);
    assert!(page3["next_cursor"].is_null());

    let (_, pdfs) = call(&app.private, req(Method::GET, "/blobs?category=document")).await;
    assert_eq!(pdfs["items"].as_array().unwrap().len(), 3);
    let (_, jpgs) = call(
        &app.private,
        req(Method::GET, "/blobs?content_type=image%2Fjpeg"),
    )
    .await;
    assert_eq!(jpgs["items"].as_array().unwrap().len(), 2);
    let (_, none) = call(&app.private, req(Method::GET, "/blobs?state=ready")).await;
    assert_eq!(none["items"].as_array().unwrap().len(), 0);
    let (status, body) = call(&app.private, req(Method::GET, "/blobs?state=bogus")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request", "{body}");

    // Out-of-range or malformed filters are rejected, not silently adjusted.
    for query in ["limit=0", "limit=201", "content_type=bogus"] {
        let (status, body) = call(&app.private, req(Method::GET, &format!("/blobs?{query}"))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["error"]["code"], "bad_request", "{query}");
    }
    let (status, _) = call(&app.private, req(Method::GET, "/blobs?limit=200")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn every_error_uses_the_json_envelope() {
    let app = setup().await;

    // Malformed JSON body.
    let (status, body) = call(
        &app.private,
        Request::builder()
            .method(Method::POST)
            .uri("/blobs")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{not json"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "bad_request");

    // Missing content type.
    let (status, body) = call(&app.private, req(Method::POST, "/blobs")).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
    assert_eq!(body["error"]["code"], "unsupported_media_type");

    // Unknown field: serde data error.
    let (status, body) = call(
        &app.private,
        json_req(Method::POST, "/blobs", json!({"bogus": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "unprocessable");

    // Path parameter that is not a UUID.
    let (status, body) = call(&app.private, req(Method::GET, "/blobs/not-a-uuid")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "bad_request");

    // Unknown route and wrong method, on both ports.
    for router in [&app.private, &app.public] {
        let (status, body) = call(router, req(Method::GET, "/nope")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"]["code"], "not_found");
        let (status, body) = call(router, req(Method::DELETE, "/healthz")).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
        assert_eq!(body["error"]["code"], "method_not_allowed");
    }
}

#[tokio::test]
async fn delete_is_soft_and_idempotent() {
    let app = setup().await;
    let (_, created) = call(&app.private, json_req(Method::POST, "/blobs", raw_blob())).await;
    let id = created["id"].as_str().unwrap();

    let (status, _) = call(&app.private, req(Method::DELETE, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app.private, req(Method::DELETE, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &app.private,
        req(Method::DELETE, &format!("/blobs/{}", uuid::Uuid::now_v7())),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = call(&app.private, req(Method::GET, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "deleted");
    assert!(body["deleted_at"].is_string());

    // Deleted blobs vanish from the public port.
    let metadata_url = public_path(created["urls"]["metadata"].as_str().unwrap());
    let (status, _) = call(&app.public, req(Method::GET, &metadata_url)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let upload_url = public_path(created["urls"]["upload"].as_str().unwrap());
    let (status, _) = call(&app.public, req(Method::PUT, &upload_url)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Upload is refused.
    let (status, _) = call(&app.private, req(Method::PUT, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Nothing was stored, so no purge job.
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(app.state.pool())
        .await
        .unwrap();
    assert_eq!(jobs, 0);
}

#[tokio::test]
async fn delete_enqueues_purge_when_stored() {
    let app = setup().await;
    let (_, created) = call(&app.private, json_req(Method::POST, "/blobs", raw_blob())).await;
    let id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE blobs SET state = 'ready', storage_key = 'k/1' WHERE id = $1")
        .bind(id)
        .execute(app.state.pool())
        .await
        .unwrap();

    let (status, _) = call(&app.private, req(Method::DELETE, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (kind, payload): (String, Value) = sqlx::query_as("SELECT kind, payload FROM jobs")
        .fetch_one(app.state.pool())
        .await
        .unwrap();
    assert_eq!(kind, "purge");
    assert_eq!(payload["storage_key"], "k/1");
}

#[tokio::test]
async fn state_machine_transitions() {
    use agrafe::blob::BlobState;
    use agrafe::blob::repo::{self, Transition};

    let app = setup().await;
    let (_, created) = call(&app.private, json_req(Method::POST, "/blobs", raw_blob())).await;
    let id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let pool = app.state.pool();
    let blob = repo::find(pool, id).await.unwrap().unwrap();

    let mut conn = pool.acquire().await.unwrap();
    let Transition::Done(uploading) = repo::begin_upload(&mut conn, &blob).await.unwrap() else {
        panic!("first upload should start");
    };
    assert_eq!(uploading.state, BlobState::Uploading);
    assert!(uploading.upload_started_at.is_some());
    assert!(
        uploading.updated_at > blob.updated_at,
        "trigger bumps updated_at"
    );

    // A concurrent PUT loses.
    assert!(matches!(
        repo::begin_upload(&mut conn, &blob).await.unwrap(),
        Transition::Conflict(_)
    ));

    // Failure hands the write back.
    assert!(matches!(
        repo::abort_upload(&mut conn, id).await.unwrap(),
        Transition::Done(_)
    ));
    assert!(matches!(
        repo::begin_upload(&mut conn, &blob).await.unwrap(),
        Transition::Done(_)
    ));

    // Once ready, write-once refuses a new upload...
    repo::transition(&mut conn, id, &[BlobState::Uploading], BlobState::Uploaded)
        .await
        .unwrap();
    repo::transition(&mut conn, id, &[BlobState::Uploaded], BlobState::Ready)
        .await
        .unwrap();
    let ready = repo::find(pool, id).await.unwrap().unwrap();
    assert!(matches!(
        repo::begin_upload(&mut conn, &ready).await.unwrap(),
        Transition::Conflict(_)
    ));

    // ...while a mutable blob accepts one.
    sqlx::query("UPDATE blobs SET write_once = false, storage_key = 'k/1' WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    let mutable = repo::find(pool, id).await.unwrap().unwrap();
    assert!(matches!(
        repo::begin_upload(&mut conn, &mutable).await.unwrap(),
        Transition::Done(_)
    ));

    // A failed re-upload keeps the previous object readable.
    let Transition::Done(after_abort) = repo::abort_upload(&mut conn, id).await.unwrap() else {
        panic!("abort should apply");
    };
    assert_eq!(after_abort.state, BlobState::Ready);

    assert!(matches!(
        repo::transition(
            &mut conn,
            uuid::Uuid::now_v7(),
            &[BlobState::Created],
            BlobState::Deleted
        )
        .await
        .unwrap(),
        Transition::Missing
    ));
}
