//! What every test builds on: the app under test, a fake clamd, request
//! builders and peeks at the database and the object store.

mod gate;

pub use gate::{At, Gate};

use agrafe::Agrafe;
use agrafe::config::Config;
use agrafe::db;
use agrafe_axum as api;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use figment::Figment;
use figment::providers::{Format, Toml};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, MutexGuard, Notify};
use tower::ServiceExt;

static LOCK: Mutex<()> = Mutex::const_new(());

/// Set once the migrations have run in this process; read under `LOCK`.
static MIGRATED: AtomicBool = AtomicBool::new(false);

pub struct TestApp {
    pub private: Router,
    pub public: Router,
    pub state: Agrafe,
    /// Holds an operation of the store in its tracks.
    pub gate: Gate,
    storage: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.storage);
    }
}

/// The test signature: the fake clamd finds any stream that contains it.
pub const VIRUS: &[u8] = b"$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!";

/// A fake clamd speaking `INSTREAM` that scans honestly.
pub async fn fake_clamd() -> SocketAddr {
    fake_clamd_in(FakeClamd::Scan).await
}

/// How the fake clamd treats a stream.
#[derive(Clone)]
pub enum FakeClamd {
    /// Reads it whole, answers `OK` or `FOUND`.
    Scan,
    /// clamd at `StreamMaxLength`: answers `ERROR` and hangs up as soon as
    /// the bytes exceed the limit.
    Limit(usize),
    /// clamd dying: hangs up without a word as soon as the bytes exceed
    /// the limit.
    Crash(usize),
    /// Reads it whole and never answers: clamd wedged.
    Hang,
    /// Reads it whole, signals `received`, then answers as `Scan` once
    /// `release` is signalled: a scan caught in the middle.
    Held {
        received: Arc<Notify>,
        release: Arc<Notify>,
    },
}

pub fn fake_clamd_on(listener: TcpListener, mode: FakeClamd) {
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mode = mode.clone();
            tokio::spawn(async move {
                let mut cmd = [0u8; 10];
                if sock.read_exact(&mut cmd).await.is_err() {
                    return;
                }
                assert_eq!(&cmd, b"zINSTREAM\0");
                let mut data = Vec::new();
                loop {
                    let mut len = [0u8; 4];
                    if sock.read_exact(&mut len).await.is_err() {
                        return;
                    }
                    let len = u32::from_be_bytes(len) as usize;
                    if len == 0 {
                        break;
                    }
                    let mut chunk = vec![0; len];
                    if sock.read_exact(&mut chunk).await.is_err() {
                        return;
                    }
                    data.extend(chunk);
                    match mode {
                        FakeClamd::Limit(limit) if data.len() > limit => {
                            let _ = sock
                                .write_all(b"INSTREAM size limit exceeded. ERROR\0")
                                .await;
                            return;
                        }
                        FakeClamd::Crash(limit) if data.len() > limit => return,
                        _ => {}
                    }
                }
                match &mode {
                    FakeClamd::Hang => std::future::pending::<()>().await,
                    FakeClamd::Held { received, release } => {
                        received.notify_one();
                        release.notified().await;
                    }
                    _ => {}
                }
                let infected = data.windows(VIRUS.len()).any(|w| w == VIRUS);
                let reply: &[u8] = if infected {
                    b"stream: Eicar-Test-Signature FOUND\0"
                } else {
                    b"stream: OK\0"
                };
                let _ = sock.write_all(reply).await;
            });
        }
    });
}

/// A fake clamd in `mode`.
pub async fn fake_clamd_in(mode: FakeClamd) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    fake_clamd_on(listener, mode);
    addr
}

/// An address nothing listens on.
pub async fn closed_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

pub async fn setup() -> TestApp {
    let clamd = fake_clamd().await;
    setup_with(clamd).await
}

pub async fn setup_with(clamd: SocketAddr) -> TestApp {
    setup_tuned(clamd, |_| {}).await
}

/// `setup_with`, with `tune` applied to the configuration first.
pub async fn setup_tuned(clamd: SocketAddr, tune: impl FnOnce(&mut Config)) -> TestApp {
    let guard = LOCK.lock().await;
    let db_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        let user = std::env::var("USER").unwrap_or_else(|_| "postgres".to_string());
        format!("postgres://{user}@localhost/agrafe_test")
    });
    let storage = std::env::temp_dir().join(format!("agrafe-test-{}", uuid::Uuid::now_v7()));
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
        [upload]
        max_size = "16 KiB"
        inline_scan_max_size = "2 KiB"
        purge_delay = "30m"
        [clamav]
        addr = "{clamd}"
        timeout = "5s"
        max_inline_scans = 1
        [jobs]
        lease_timeout = "10m"
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
    // From the string alone: `AGRAFE_` variables in the environment must not
    // change what the tests run against.
    let mut config: Config = Figment::from(Toml::string(&toml)).extract().unwrap();
    // The helpers are the workspace's own binaries; previews are kept small
    // to keep the fixtures small.
    config.media.render.extract_command = helper("agrafe-extract");
    config.media.render.render_text_command = helper("agrafe-render-text");
    config.media.render.preview.max_dimension = 400;
    tune(&mut config);

    let pool = db::connect(&config.database).await.unwrap();
    if !MIGRATED.load(Ordering::Relaxed) {
        db::migrate(&pool).await.unwrap();
        MIGRATED.store(true, Ordering::Relaxed);
    }
    sqlx::query("TRUNCATE jobs, attachments, blobs")
        .execute(&pool)
        .await
        .unwrap();

    let gate = Gate::default();
    let store = agrafe::storage::build(&config.storage).unwrap();
    let store = Arc::new(gate::GatedStore::new(store, gate.clone()));
    let state = Agrafe::with_store(config, pool, store).unwrap();
    TestApp {
        private: api::private_router(state.clone()),
        public: api::public_router(state.clone()),
        state,
        gate,
        storage,
        _guard: guard,
    }
}

pub async fn call(router: &Router, req: Request<Body>) -> (StatusCode, Value) {
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

pub fn json_req(method: Method, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub fn req(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

pub fn raw_blob() -> Value {
    json!({
        "filename": "photo été.jpg",
        "content_type": "image/jpeg",
        "input_type": "raw",
        "checksum": STANDARD.encode([1u8; 32]),
        "size": 1024
    })
}

/// Strip the base so the URL can be sent to the public router.
pub fn public_path(url: &str) -> String {
    url.strip_prefix("https://agrafe.test").unwrap().to_string()
}

/// A blob's minted `kind` URL, as a path for the public router.
pub fn public_url(blob: &Value, kind: &str) -> String {
    public_path(blob["urls"][kind].as_str().unwrap())
}

/// The id of a blob as the API returned it.
pub fn id_of(blob: &Value) -> &str {
    blob["id"].as_str().unwrap()
}

pub fn parse_id(id: &str) -> uuid::Uuid {
    id.parse().unwrap()
}

/// A PNG header followed by filler, long enough to span several 1 KiB
/// encryption segments.
pub fn png_bytes(len: usize) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    v.extend((v.len()..len).map(|i| (i * 31 % 251) as u8));
    v
}

/// A blob declaration matching `bytes`.
pub fn blob_for(bytes: &[u8], content_type: &str, input_type: &str) -> Value {
    json!({
        "filename": "photo.png",
        "content_type": content_type,
        "input_type": input_type,
        "checksum": STANDARD.encode(Sha256::digest(bytes)),
        "size": bytes.len()
    })
}

pub async fn create(app: &TestApp, blob: Value) -> Value {
    let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", blob)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

/// A raw `PUT` of `bytes` to the blob on the private port.
pub async fn put_private(app: &TestApp, id: &str, bytes: Vec<u8>) -> (StatusCode, Value) {
    call(&app.private, put_raw(&format!("/blobs/{id}"), bytes)).await
}

/// `DELETE` the blob on the private port.
pub async fn delete(app: &TestApp, id: impl std::fmt::Display) -> StatusCode {
    call(&app.private, req(Method::DELETE, &format!("/blobs/{id}")))
        .await
        .0
}

pub fn put_raw(uri: &str, bytes: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method(Method::PUT)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(bytes))
        .unwrap()
}

/// A raw `PUT` whose body arrives in pieces of `piece` bytes and carries no
/// `Content-Length`, the way a browser streams a file.
pub fn put_raw_chunked(uri: &str, bytes: Vec<u8>, piece: usize) -> Request<Body> {
    let pieces: Vec<Result<bytes::Bytes, std::io::Error>> = bytes
        .chunks(piece)
        .map(|p| Ok(bytes::Bytes::copy_from_slice(p)))
        .collect();
    Request::builder()
        .method(Method::PUT)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from_stream(futures::stream::iter(pieces)))
        .unwrap()
}

pub fn put_multipart(uri: &str, parts: &[(&str, &[u8])]) -> Request<Body> {
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
pub async fn jobs_for(app: &TestApp, id: &str) -> Vec<String> {
    let id: uuid::Uuid = parse_id(id);
    sqlx::query_scalar("SELECT kind FROM jobs WHERE blob_id = $1 ORDER BY kind")
        .bind(id)
        .fetch_all(app.state.pool())
        .await
        .unwrap()
}

/// Every object in the store.
pub async fn stored_objects(app: &TestApp) -> Vec<object_store::ObjectMeta> {
    use futures::TryStreamExt;
    app.state.store().list(None).try_collect().await.unwrap()
}

/// Poll the private port until the blob reaches `want`, or give up.
pub async fn wait_for_state(app: &TestApp, id: &str, want: &str) {
    let mut state = String::new();
    for _ in 0..100 {
        let got = get_blob(app, id).await;
        state = got["state"].as_str().unwrap().to_string();
        if state == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("blob {id} is {state}, never {want}");
}

/// The `last_error` of the blob's one job.
pub async fn last_error(app: &TestApp, id: &str) -> String {
    sqlx::query_scalar("SELECT last_error FROM jobs WHERE blob_id = $1")
        .bind(parse_id(id))
        .fetch_one(app.state.pool())
        .await
        .unwrap()
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The object key a blob's original is stored under.
pub async fn storage_key(app: &TestApp, id: &str) -> String {
    agrafe::blob::repo::find(app.state.pool(), parse_id(id))
        .await
        .unwrap()
        .unwrap()
        .storage_key
        .expect("storage_key")
}

/// Read and decrypt the stored original straight from the object store.
pub async fn stored_plaintext(app: &TestApp, id: &str) -> Vec<u8> {
    use agrafe::blob::repo;
    use agrafe_crypto::streaming::{self, Layout};
    use object_store::ObjectStoreExt;

    let blob = repo::find(app.state.pool(), parse_id(id))
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

/// A raw response: status, headers and body bytes.
pub async fn fetch(
    router: &Router,
    req: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, headers, bytes)
}

pub fn get_with(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder().method(Method::GET).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).unwrap()
}

/// Upload `bytes` as a ready blob and return its metadata.
pub async fn ready_blob(app: &TestApp, bytes: &[u8]) -> Value {
    let mut blob = blob_for(bytes, "image/png", "raw");
    blob["skip_virus_scan"] = json!(true);
    let created = create(app, blob).await;
    let id = id_of(&created);
    let (status, body) = put_private(app, id, bytes.to_vec()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// A file of `len` bytes carrying the test signature across the end of the
/// first 1 KiB segment (968 plaintext bytes), so a scan that skipped a
/// segment or split the stream at segment edges would miss it.
pub fn infected_png(len: usize) -> Vec<u8> {
    const AT: usize = 950;
    let mut bytes = png_bytes(len);
    bytes[AT..AT + VIRUS.len()].copy_from_slice(VIRUS);
    bytes
}

pub async fn get_blob(app: &TestApp, id: &str) -> Value {
    let (status, body) = call(&app.private, req(Method::GET, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

pub async fn run_scan_job(app: &TestApp) -> bool {
    run_job(app, agrafe::jobs::JobKind::Scan).await
}

/// Run one job of `kind`, returning whether there was one.
pub async fn run_job(app: &TestApp, kind: agrafe::jobs::JobKind) -> bool {
    agrafe::jobs::runner::run_one(&app.state, kind, "test")
        .await
        .unwrap()
}

/// A file from `fixtures/` at the workspace root, built by `make.py` there.
pub fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// A helper binary of this workspace, next to this test executable's
/// directory (`target/<profile>/deps/api-*` sits below the binaries).
/// `cargo test` at the workspace root builds them; `cargo test -p agrafe`
/// and `cargo llvm-cov` do not, so they are built here when missing, into
/// the target directory this test runs from.
pub fn helper(name: &str) -> String {
    static BUILT: std::sync::Once = std::sync::Once::new();
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let path = dir.join(name);
    if !path.exists() {
        BUILT.call_once(|| {
            let status = std::process::Command::new(env!("CARGO"))
                .args(["build", "-p", "agrafe-extract", "-p", "agrafe-render-text"])
                .arg("--target-dir")
                .arg(dir.parent().unwrap())
                .status()
                .expect("run cargo build for the helpers");
            assert!(status.success(), "building the helpers failed");
        });
    }
    assert!(path.exists(), "helper {} not found", path.display());
    path.to_string_lossy().into_owned()
}

/// Upload `bytes` as a ready blob named `filename` and return its metadata.
pub async fn ready_file(app: &TestApp, filename: &str, content_type: &str, bytes: &[u8]) -> Value {
    let mut blob = blob_for(bytes, content_type, "raw");
    blob["filename"] = json!(filename);
    blob["skip_virus_scan"] = json!(true);
    let created = create(app, blob).await;
    let id = id_of(&created);
    let (status, body) = put_private(app, id, bytes.to_vec()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// Width and height of a WebP, from its container header.
pub fn webp_size(bytes: &[u8]) -> (u32, u32) {
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WEBP");
    match &bytes[12..16] {
        b"VP8 " => {
            assert_eq!(&bytes[23..26], &[0x9d, 0x01, 0x2a], "VP8 key frame");
            let w = u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff;
            let h = u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff;
            (u32::from(w), u32::from(h))
        }
        b"VP8L" => {
            assert_eq!(bytes[20], 0x2f, "VP8L signature");
            let b: Vec<u32> = bytes[21..25].iter().map(|b| u32::from(*b)).collect();
            let w = (b[0] | ((b[1] & 0x3f) << 8)) + 1;
            let h = ((b[1] >> 6) | (b[2] << 2) | ((b[3] & 0x0f) << 10)) + 1;
            (w, h)
        }
        b"VP8X" => {
            let w = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]) + 1;
            let h = u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]) + 1;
            (w, h)
        }
        other => panic!("unknown WebP chunk {other:?}"),
    }
}
