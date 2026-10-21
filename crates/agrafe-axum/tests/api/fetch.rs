//! `url` blobs: the service downloads the file from a URL the backend
//! gives at creation, through the fetch job, instead of receiving it
//! through a `PUT`. The remote is an in-process listener on localhost
//! speaking just enough HTTP/1.1 to script every answer.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agrafe::jobs::JobKind;
use agrafe::jobs::reap::{self, Report};
use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jiff::Timestamp;
use md5::Md5;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::support::*;

/// What the remote was asked.
pub struct Hit {
    pub path: String,
    /// The request head, lowercased, for looking at headers.
    pub head: String,
    /// Connections before this one.
    pub n: usize,
}

/// A remote on localhost answering every connection with what `reply`
/// says, then closing it. Returns its base URL.
async fn remote(reply: impl Fn(&Hit) -> Vec<u8> + Send + Sync + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let reply = Arc::new(reply);
    let hits = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let reply = reply.clone();
            let n = hits.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let read = sock.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..read]);
                }
                let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
                let path = String::from_utf8_lossy(&buf)
                    .lines()
                    .next()
                    .and_then(|line| line.split(' ').nth(1))
                    .unwrap_or("/")
                    .to_string();
                let bytes = reply(&Hit { path, head, n });
                let _ = sock.write_all(&bytes).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

fn response(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\nConnection: close\r\n").into_bytes();
    for (name, value) in headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

/// `200` with the body and its length.
fn ok(body: &[u8]) -> Vec<u8> {
    response(
        "200 OK",
        &[("Content-Length", body.len().to_string())],
        body,
    )
}

/// `200` with the body in one chunk and no `Content-Length`.
fn chunked(body: &[u8]) -> Vec<u8> {
    let mut framed = format!("{:x}\r\n", body.len()).into_bytes();
    framed.extend_from_slice(body);
    framed.extend_from_slice(b"\r\n0\r\n\r\n");
    response(
        "200 OK",
        &[("Transfer-Encoding", "chunked".into())],
        &framed,
    )
}

fn status(status: &str, headers: &[(&str, String)]) -> Vec<u8> {
    response(status, headers, b"")
}

fn redirect(to: &str) -> Vec<u8> {
    status("302 Found", &[("Location", to.to_string())])
}

/// `200` announcing `declared` bytes and sending fewer.
fn short(declared: usize, body: &[u8]) -> Vec<u8> {
    response("200 OK", &[("Content-Length", declared.to_string())], body)
}

/// An app whose fetches may reach localhost over plain HTTP.
async fn setup_fetching(db: TestDatabase) -> TestApp {
    let clamd = fake_clamd().await;
    setup_tuned(db, clamd, |c| c.fetch.allow_private_networks = true).await
}

/// A `url` blob declaration for `url`, scanning skipped.
fn url_blob(url: &str) -> Value {
    json!({
        "filename": "photo.png",
        "content_type": "image/png",
        "input_type": "url",
        "url": url,
        "skip_virus_scan": true
    })
}

fn md5_of(bytes: &[u8]) -> String {
    STANDARD.encode(Md5::digest(bytes))
}

fn sha_of(bytes: &[u8]) -> String {
    STANDARD.encode(Sha256::digest(bytes))
}

/// The blob's one fetch job: attempts, dead, last error.
async fn fetch_job(app: &TestApp, id: &str) -> (i32, bool, String) {
    sqlx::query_as(
        "SELECT attempts, failed_at IS NOT NULL, coalesce(last_error, '') FROM jobs \
         WHERE blob_id = $1 AND kind = 'fetch'",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap()
}

/// Seconds until the blob's fetch job is due.
async fn fetch_due_in(app: &TestApp, id: &str) -> f64 {
    sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM run_at - now())::float8 FROM jobs \
         WHERE kind = 'fetch' AND blob_id = $1",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap()
}

async fn make_due(app: &TestApp, id: &str) {
    sqlx::query("UPDATE jobs SET run_at = now() WHERE blob_id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_url_blob_is_fetched_by_a_job() {
    let db = TestDatabase::new().await;
    let app = setup_fetching(db).await;
    let bytes = png_bytes(1500);
    let body = bytes.clone();
    let base = remote(move |hit| {
        assert!(hit.head.contains("user-agent: agrafe/"), "{}", hit.head);
        assert!(!hit.head.contains("cookie"), "{}", hit.head);
        assert_eq!(hit.path, "/files/secret-token-123");
        ok(&body)
    })
    .await;
    let url = format!("{base}/files/secret-token-123");

    let mut blob = url_blob(&url);
    blob["skip_virus_scan"] = json!(false);
    let created = create(&app, blob).await;
    let id = id_of(&created);
    assert_eq!(created["state"], "created");
    assert_eq!(created["input_type"], "url");
    assert_eq!(created["url"], url, "reported on the private port");
    assert_eq!(created["checksum"], json!(null));
    assert_eq!(created["size"], json!(null));
    assert_eq!(created["public_upload_allowed"], false);
    assert_eq!(created["urls"]["upload"], json!(null), "{created}");
    assert_eq!(created["source_parts"], json!(null), "not a generated blob");
    assert_eq!(jobs_for(&app, id).await, ["fetch"]);

    assert!(run_job(&app, JobKind::Fetch).await);
    assert!(!run_job(&app, JobKind::Fetch).await, "the job is done");
    let got = get_blob(&app, id).await;
    // Small enough for the inline scan: straight to ready.
    assert_eq!(got["state"], "ready", "{got}");
    assert_eq!(got["scan_state"], "clean");
    assert_eq!(
        got["checksum"],
        sha_of(&bytes),
        "computed, none was declared"
    );
    assert_eq!(got["checksum_md5"], md5_of(&bytes));
    assert_eq!(got["size"], bytes.len());
    assert_eq!(
        got["url"],
        json!(null),
        "a presigned secret, cleared once the bytes are in"
    );
    assert!(got["uploaded_at"].is_string());
    assert_eq!(jobs_for(&app, id).await, ["render"]);
    assert_eq!(stored_plaintext(&app, id).await, bytes);

    // The public port sees a blob like any other, and no URL.
    let (status, public) = call(&app.public, req(Method::GET, &public_url(&got, "metadata"))).await;
    assert_eq!(status, StatusCode::OK, "{public}");
    assert_eq!(public["state"], "ready");
    assert!(public.get("url").is_none(), "{public}");
}

#[tokio::test]
async fn declared_checksums_and_size_are_checked() {
    let db = TestDatabase::new().await;
    let app = setup_fetching(db).await;
    let bytes = png_bytes(3000);
    let body = bytes.clone();
    let base = remote(move |_| ok(&body)).await;
    let url = format!("{base}/photo.png");

    // Everything declared and right: fetched, the row holds what was
    // declared, and a file this size waits for the scan job.
    let mut blob = url_blob(&url);
    blob["skip_virus_scan"] = json!(false);
    blob["checksum"] = json!(sha_of(&bytes));
    blob["checksum_md5"] = json!(md5_of(&bytes));
    blob["size"] = json!(bytes.len());
    let created = create(&app, blob).await;
    let id = id_of(&created);
    assert_eq!(created["checksum_md5"], md5_of(&bytes), "as declared");
    assert!(run_job(&app, JobKind::Fetch).await);
    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "uploaded", "{got}");
    assert_eq!(jobs_for(&app, id).await, ["scan"]);
    assert!(run_scan_job(&app).await);
    assert_eq!(get_blob(&app, id).await["state"], "ready");

    // Each declaration wrong in turn: dead at once, nothing stored, the
    // blob back in `created` with the verdict, and the URL in none of it.
    let objects_before = stored_objects(&app).await.len();
    let wrong_md5 = {
        let mut b = url_blob(&url);
        b["checksum_md5"] = json!(STANDARD.encode([9u8; 16]));
        b
    };
    let wrong_sha = {
        let mut b = url_blob(&url);
        b["checksum"] = json!(STANDARD.encode([9u8; 32]));
        b
    };
    let wrong_size = {
        let mut b = url_blob(&url);
        b["size"] = json!(bytes.len() + 1);
        b
    };
    for (blob, says) in [
        (wrong_md5, "MD5 does not match"),
        (wrong_sha, "checksum does not match"),
        (
            wrong_size,
            "Content-Length 3000 does not match the declared size 3001",
        ),
    ] {
        let created = create(&app, blob).await;
        let id = id_of(&created);
        assert!(run_job(&app, JobKind::Fetch).await);
        let (attempts, dead, error) = fetch_job(&app, id).await;
        assert_eq!((attempts, dead), (1, true), "{says}: {error}");
        assert!(error.contains(says), "{says}: {error}");
        assert!(!error.contains("photo.png"), "the URL stays out: {error}");
        let got = get_blob(&app, id).await;
        assert_eq!(got["state"], "created", "{got}");
        assert!(got["failed_at"].is_string(), "{got}");
        assert!(got["last_error"].as_str().unwrap().contains(says), "{got}");
    }
    assert_eq!(stored_objects(&app).await.len(), objects_before);
}

#[tokio::test]
async fn the_bytes_are_sniffed_and_scanned_like_an_upload() {
    let db = TestDatabase::new().await;
    let app = setup_fetching(db).await;
    let pdf = fixture("image.pdf");
    let infected = infected_png(1500);
    let (pdf_body, infected_body) = (pdf.clone(), infected.clone());
    let base = remote(move |hit| match hit.path.as_str() {
        "/pdf" => ok(&pdf_body),
        "/virus" => ok(&infected_body),
        other => panic!("{other}"),
    })
    .await;

    // Declared a PNG, is a PDF.
    let created = create(&app, url_blob(&format!("{base}/pdf"))).await;
    let id = id_of(&created);
    assert!(run_job(&app, JobKind::Fetch).await);
    let (_, dead, error) = fetch_job(&app, id).await;
    assert!(dead, "{error}");
    assert!(error.contains("looks like"), "{error}");

    // Infected: stored, the blob is `infected`, the job is done.
    let mut blob = url_blob(&format!("{base}/virus"));
    blob["skip_virus_scan"] = json!(false);
    let created = create(&app, blob).await;
    let id = id_of(&created);
    assert!(run_job(&app, JobKind::Fetch).await);
    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "infected", "{got}");
    assert_eq!(got["scan_signature"], "Eicar-Test-Signature");
    assert_eq!(got["failed_at"], json!(null));
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());
}

#[tokio::test]
async fn a_server_error_is_retried_and_a_client_error_is_not() {
    let db = TestDatabase::new().await;
    let app = setup_fetching(db).await;
    let bytes = png_bytes(500);
    let body = bytes.clone();
    let base = remote(move |hit| match (hit.path.as_str(), hit.n) {
        ("/flaky", 0) => status("500 Internal Server Error", &[]),
        ("/flaky", _) => ok(&body),
        ("/gone", _) => status("404 Not Found", &[]),
        ("/teapot", _) => status("418 I'm a teapot", &[]),
        other => panic!("{other:?}"),
    })
    .await;

    let created = create(&app, url_blob(&format!("{base}/flaky"))).await;
    let id = id_of(&created);
    assert!(run_job(&app, JobKind::Fetch).await);
    let (attempts, dead, error) = fetch_job(&app, id).await;
    assert_eq!((attempts, dead), (1, false), "{error}");
    assert!(error.contains("500"), "{error}");
    let due = fetch_due_in(&app, id).await;
    assert!((25.0..=30.0).contains(&due), "the backoff: {due}");
    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "created");
    assert_eq!(got["failed_at"], json!(null), "not dead yet");
    make_due(&app, id).await;
    assert!(run_job(&app, JobKind::Fetch).await);
    assert_eq!(get_blob(&app, id).await["state"], "ready");
    assert_eq!(stored_plaintext(&app, id).await, bytes);

    for path in ["gone", "teapot"] {
        let created = create(&app, url_blob(&format!("{base}/{path}"))).await;
        let id = id_of(&created);
        assert!(run_job(&app, JobKind::Fetch).await);
        let (attempts, dead, error) = fetch_job(&app, id).await;
        assert_eq!((attempts, dead), (1, true), "{path}: {error}");
        assert!(error.contains("the remote answered 4"), "{error}");
    }
}

#[tokio::test]
async fn retry_after_is_honoured_up_to_a_point() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.fetch.allow_private_networks = true;
        c.fetch.max_retry_after = Duration::from_secs(900);
    })
    .await;
    let base = remote(|hit| match hit.path.as_str() {
        "/soon" => status("429 Too Many Requests", &[("Retry-After", "120".into())]),
        "/later" => status("503 Service Unavailable", &[("Retry-After", "3600".into())]),
        "/unsaid" => status("429 Too Many Requests", &[]),
        other => panic!("{other}"),
    })
    .await;

    let created = create(&app, url_blob(&format!("{base}/soon"))).await;
    let id = id_of(&created);
    assert!(run_job(&app, JobKind::Fetch).await);
    let (attempts, dead, _) = fetch_job(&app, id).await;
    assert_eq!((attempts, dead), (1, false));
    let due = fetch_due_in(&app, id).await;
    assert!((115.0..=120.0).contains(&due), "{due}");

    let created = create(&app, url_blob(&format!("{base}/unsaid"))).await;
    let id = id_of(&created);
    assert!(run_job(&app, JobKind::Fetch).await);
    let due = fetch_due_in(&app, id).await;
    assert!((25.0..=30.0).contains(&due), "the backoff alone: {due}");

    let created = create(&app, url_blob(&format!("{base}/later"))).await;
    let id = id_of(&created);
    assert!(run_job(&app, JobKind::Fetch).await);
    let (attempts, dead, error) = fetch_job(&app, id).await;
    assert_eq!((attempts, dead), (1, true), "{error}");
    assert!(error.contains("beyond fetch.max_retry_after"), "{error}");
}

#[tokio::test]
async fn redirects_are_followed_under_the_same_checks() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.fetch.allow_private_networks = true;
        c.fetch.max_redirects = 2;
    })
    .await;
    let bytes = png_bytes(500);
    let body = bytes.clone();
    let base = remote(move |hit| match hit.path.as_str() {
        "/one" => redirect("/two"),
        "/two" => redirect("/file"),
        "/file" => ok(&body),
        "/far" => redirect("/one"),
        "/loop" => redirect("/loop"),
        "/creds" => redirect("http://user:pw@127.0.0.1/file"),
        "/ftp" => redirect("ftp://127.0.0.1/file"),
        other => panic!("{other}"),
    })
    .await;

    let created = create(&app, url_blob(&format!("{base}/one"))).await;
    let id = id_of(&created);
    assert!(run_job(&app, JobKind::Fetch).await);
    assert_eq!(get_blob(&app, id).await["state"], "ready");
    assert_eq!(stored_plaintext(&app, id).await, bytes);

    for (path, says) in [
        ("far", "more than 2 redirects"),
        ("loop", "more than 2 redirects"),
        ("creds", "must not carry credentials"),
        ("ftp", "must use https"),
    ] {
        let created = create(&app, url_blob(&format!("{base}/{path}"))).await;
        let id = id_of(&created);
        assert!(run_job(&app, JobKind::Fetch).await);
        let (attempts, dead, error) = fetch_job(&app, id).await;
        assert_eq!((attempts, dead), (1, true), "{path}: {error}");
        assert!(error.contains(says), "{path}: {error}");
        assert_eq!(get_blob(&app, id).await["state"], "created");
    }
}

/// Without `allow_private_networks`, a literal private address is refused
/// at creation, and a name that resolves to one kills the fetch at once.
/// A name that does not resolve is retried: the resolver may be the one
/// having a bad minute.
#[tokio::test]
async fn the_network_policy_refuses_private_addresses() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    for (url, says) in [
        ("http://files.example/a", "must use https"),
        ("https://user:pw@files.example/a", "credentials"),
        ("https://127.0.0.1/a", "refused network"),
        (
            "https://169.254.169.254/latest/meta-data/",
            "refused network",
        ),
        ("https://[fd00::1]/a", "refused network"),
        ("nope", "does not parse"),
    ] {
        let (status, body) = call(
            &app.private,
            json_req(Method::POST, "/blobs", url_blob(url)),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{url}: {body}");
        assert!(
            body["error"]["message"].as_str().unwrap().contains(says),
            "{url}: {body}"
        );
    }
    let (status, body) = call(
        &app.private,
        json_req(
            Method::POST,
            "/blobs",
            json!({"filename": "a", "content_type": "image/png", "input_type": "url"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("url is required"),
        "{body}"
    );
    let mut not_url = raw_blob();
    not_url["url"] = json!("https://files.example/a");
    let (status, _) = call(&app.private, json_req(Method::POST, "/blobs", not_url)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A name is checked when it resolves: localhost does, to loopback.
    for (url, says, permanent) in [
        ("https://localhost:1/a", "refused address", true),
        ("https://no-such-host.invalid/a", "does not resolve", false),
    ] {
        let created = create(&app, url_blob(url)).await;
        let id = id_of(&created);
        assert!(run_job(&app, JobKind::Fetch).await);
        let (attempts, dead, error) = fetch_job(&app, id).await;
        assert_eq!((attempts, dead), (1, permanent), "{url}: {error}");
        assert!(error.contains(says), "{url}: {error}");
        assert_eq!(get_blob(&app, id).await["state"], "created");
    }
}

#[tokio::test]
async fn a_body_cut_short_is_retried_and_one_over_the_cap_is_not() {
    let db = TestDatabase::new().await;
    let app = setup_fetching(db).await;
    let base = remote(|hit| match hit.path.as_str() {
        "/short" => short(5000, &png_bytes(1000)),
        "/huge" => chunked(&png_bytes(20 * 1024)),
        other => panic!("{other}"),
    })
    .await;

    let created = create(&app, url_blob(&format!("{base}/short"))).await;
    let id = id_of(&created);
    assert!(run_job(&app, JobKind::Fetch).await);
    let (attempts, dead, error) = fetch_job(&app, id).await;
    assert_eq!((attempts, dead), (1, false), "{error}");
    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "created", "handed back: {got}");
    assert!(stored_objects(&app).await.is_empty(), "nothing stored");

    // No size declared: the cap is upload.max_size, 16 KiB here.
    let created = create(&app, url_blob(&format!("{base}/huge"))).await;
    let id = id_of(&created);
    assert!(run_job(&app, JobKind::Fetch).await);
    let (attempts, dead, error) = fetch_job(&app, id).await;
    assert_eq!((attempts, dead), (1, true), "{error}");
    assert!(
        error.contains("exceeds the maximum of 16384 bytes"),
        "{error}"
    );
    assert!(stored_objects(&app).await.is_empty());
}

/// A remote that answers and then goes quiet mid-body is cut off by the
/// read timeout, well before the whole-response one, and retried: it must
/// not hold a fetch worker for as long as it cares to stall.
#[tokio::test]
async fn a_stalled_body_is_cut_off_and_retried() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.fetch.allow_private_networks = true;
        c.fetch.read_timeout = Duration::from_secs(1);
    })
    .await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let head = short(5000, &png_bytes(1000));
                let _ = sock.write_all(&head).await;
                // Hold the connection open, sending nothing more.
                tokio::time::sleep(Duration::from_secs(60)).await;
            });
        }
    });

    let created = create(&app, url_blob(&format!("{base}/stalls"))).await;
    let id = id_of(&created);
    let started = std::time::Instant::now();
    assert!(run_job(&app, JobKind::Fetch).await);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    let (attempts, dead, error) = fetch_job(&app, id).await;
    assert_eq!((attempts, dead), (1, false), "{error}");
    assert!(!error.contains("stalls"), "the URL stays out: {error}");
    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "created", "handed back: {got}");
    assert!(stored_objects(&app).await.is_empty(), "nothing stored");
}

#[tokio::test]
async fn a_url_blob_takes_no_put_parts_or_finish() {
    let db = TestDatabase::new().await;
    let app = setup_fetching(db).await;
    let created = create(&app, url_blob("http://127.0.0.1:1/a")).await;
    let id = id_of(&created);

    let (status, body) = put_private(&app, id, png_bytes(100)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "conflict");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("fetched by the service"),
        "{body}"
    );
    let (status, body) = call(
        &app.private,
        put_raw(&format!("/blobs/{id}/parts/1"), b"[]\n".to_vec()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = call(
        &app.private,
        req(Method::POST, &format!("/blobs/{id}/finish")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(get_blob(&app, id).await["state"], "created");
    assert_eq!(jobs_for(&app, id).await, ["fetch"]);
}

/// A fetch finding the row `uploading` has met itself, cut short on a
/// crashed worker and not yet reset by the reaper: it is retried, not
/// dropped, so the blob is never left with no job at all, and the retry
/// is due once the reaper has had the time to reset the row, so one
/// attempt covers the wait.
#[tokio::test]
async fn an_uploading_blob_is_a_transient_failure() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.fetch.allow_private_networks = true;
        c.upload.stale_after = Duration::from_secs(600);
        c.jobs.reap_interval = Duration::from_secs(60);
    })
    .await;
    let created = create(&app, url_blob("http://127.0.0.1:1/a")).await;
    let id = id_of(&created);
    sqlx::query(
        "UPDATE blobs SET state = 'uploading', upload_started_at = now(), \
         upload_heartbeat_at = now() WHERE id = $1",
    )
    .bind(parse_id(id))
    .execute(app.state.pool())
    .await
    .unwrap();
    assert!(run_job(&app, JobKind::Fetch).await);
    let (attempts, dead, error) = fetch_job(&app, id).await;
    assert_eq!((attempts, dead), (1, false), "{error}");
    assert!(error.contains("uploading"), "{error}");
    let due = fetch_due_in(&app, id).await;
    assert!(
        (650.0..=660.0).contains(&due),
        "stale_after + reap_interval: {due}"
    );

    // Any other state is nothing to do.
    sqlx::query("UPDATE blobs SET state = 'ready' WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    make_due(&app, id).await;
    assert!(run_job(&app, JobKind::Fetch).await);
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());
}

#[tokio::test]
async fn a_dead_fetch_is_retried_from_the_blob() {
    let db = TestDatabase::new().await;
    let app = setup_fetching(db).await;
    let bytes = png_bytes(500);
    let body = bytes.clone();
    let base = remote(move |hit| match hit.n {
        0 => status("403 Forbidden", &[]),
        _ => ok(&body),
    })
    .await;
    let created = create(&app, url_blob(&format!("{base}/expired-then-renewed"))).await;
    let id = id_of(&created);
    assert!(run_job(&app, JobKind::Fetch).await);
    let (_, dead, _) = fetch_job(&app, id).await;
    assert!(dead);

    // The verdict is on the blob, where the backend looks.
    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "created");
    assert!(got["failed_at"].is_string(), "{got}");
    assert!(
        got["last_error"]
            .as_str()
            .unwrap()
            .contains("the remote answered 403"),
        "{got}"
    );
    let (_, failed) = call(&app.private, req(Method::GET, "/blobs?failed=true")).await;
    assert_eq!(failed["items"][0]["id"], id, "{failed}");

    // The retry enqueues a fresh fetch, the dead job goes, the failure is
    // cleared; the backend having renewed the URL, it goes through.
    let (status, body) = call(
        &app.private,
        req(Method::POST, &format!("/blobs/{id}/retry")),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["failed_at"], json!(null));
    assert_eq!(body["last_error"], json!(null));
    assert_eq!(jobs_for(&app, id).await, ["fetch"]);
    let (attempts, dead, _) = fetch_job(&app, id).await;
    assert_eq!((attempts, dead), (0, false), "a new job");
    assert!(run_job(&app, JobKind::Fetch).await);
    assert_eq!(get_blob(&app, id).await["state"], "ready");
    assert_eq!(stored_plaintext(&app, id).await, bytes);

    // Retried once more, a ready blob waits on nothing.
    let (status, _) = call(
        &app.private,
        req(Method::POST, &format!("/blobs/{id}/retry")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// A migration creates attached, so the blob has no expiry; one whose
/// fetch gave up is caught by the abandoned rule a day after the failure.
#[tokio::test]
async fn an_attached_blob_whose_fetch_died_is_expired_as_abandoned() {
    let db = TestDatabase::new().await;
    let app = setup_fetching(db).await;
    let pool = app.state.pool();
    let base = remote(|_| status("404 Not Found", &[])).await;
    let mut blob = url_blob(&format!("{base}/gone"));
    blob["attachment"] = json!({"record_type": "Dossier", "record_id": "1", "name": "piece"});
    let created = create(&app, blob).await;
    let id = id_of(&created);
    assert_eq!(created["expires_at"], json!(null));
    assert!(run_job(&app, JobKind::Fetch).await);
    let got = get_blob(&app, id).await;
    assert!(got["failed_at"].is_string(), "{got}");

    // Seen within the day from the failure.
    let (_, listing) = call(
        &app.private,
        req(Method::GET, "/attachments?record_type=Dossier"),
    )
    .await;
    assert_eq!(listing["items"][0]["blob_state"], "created", "{listing}");
    assert!(
        listing["items"][0]["blob_failed_at"].is_string(),
        "{listing}"
    );
    assert_eq!(reap_now(&app).await, Report::default(), "not yet");

    // The failure touched the row; a day later it is abandoned.
    sqlx::query("ALTER TABLE blobs DISABLE TRIGGER blobs_updated_at")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE blobs SET updated_at = now() - interval '2 days' WHERE id = $1")
        .bind(parse_id(id))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE blobs ENABLE TRIGGER blobs_updated_at")
        .execute(pool)
        .await
        .unwrap();
    let report = reap_now(&app).await;
    assert_eq!(report.abandoned, 1, "{report:?}");
    assert_eq!(report.purges, 0, "nothing of it is in the bucket");
    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "expired", "{got}");
    assert_eq!(got["failed_at"], json!(null), "terminal: nothing to finish");
    let (_, listing) = call(
        &app.private,
        req(Method::GET, "/attachments?record_type=Dossier"),
    )
    .await;
    assert_eq!(listing["items"][0]["blob_state"], "expired", "{listing}");

    // The fetch of an expired blob is nothing to do.
    let (status, _) = call(
        &app.private,
        req(Method::POST, &format!("/blobs/{id}/retry")),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
}

/// A blob past its expiry is not fetched, reaper or no reaper: the remote
/// is never contacted and the job is done.
#[tokio::test]
async fn an_expired_blob_is_not_fetched() {
    let db = TestDatabase::new().await;
    let app = setup_fetching(db).await;
    let base = remote(|hit| panic!("fetched {}", hit.path)).await;
    let created = create(&app, url_blob(&format!("{base}/expired"))).await;
    let id = id_of(&created);
    sqlx::query("UPDATE blobs SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    assert!(run_job(&app, JobKind::Fetch).await);
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new(), "done");
    // The row is left for the reaper to record the expiry.
    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "created", "{got}");
    assert_eq!(got["failed_at"], json!(null));
    assert!(stored_objects(&app).await.is_empty());
    assert_eq!(reap_now(&app).await.expired, 1);
    assert_eq!(get_blob(&app, id).await["state"], "expired");
}

async fn reap_now(app: &TestApp) -> Report {
    reap::reap(&app.state, Timestamp::now()).await.unwrap()
}

/// A `raw` upload may declare its MD5 too, and it is checked.
#[tokio::test]
async fn a_direct_upload_may_declare_its_md5() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(500);
    let mut blob = blob_for(&bytes, "image/png", "raw");
    blob["skip_virus_scan"] = json!(true);
    blob["checksum_md5"] = json!(md5_of(&bytes));
    let created = create(&app, blob.clone()).await;
    let (status, body) = put_private(&app, id_of(&created), bytes.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["checksum_md5"], md5_of(&bytes));

    blob["checksum_md5"] = json!(STANDARD.encode([9u8; 16]));
    let created = create(&app, blob).await;
    let (status, body) = put_private(&app, id_of(&created), bytes).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("MD5 does not match"),
        "{body}"
    );
    assert_eq!(get_blob(&app, id_of(&created)).await["state"], "created");
}
