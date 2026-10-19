//! The webhook: one `notify` job per event, delivered to the configured
//! URL with a Standard Webhooks signature, retried through the queue.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

use agrafe::config::Config;
use agrafe::jobs::notify::Event;
use agrafe::jobs::{self, JobKind, reap};
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::routing::{any, post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use jiff::Timestamp;
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::support::*;

/// 24 random-looking bytes, as `openssl rand -base64 24` would give.
const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

/// One request the receiver got.
struct Hit {
    headers: HeaderMap,
    body: Value,
    raw: String,
}

/// A receiver on localhost answering every `POST` with `status`, and a
/// file on `/file` for a fetch to pick up.
struct Receiver {
    url: String,
    file_url: String,
    status: Arc<AtomicU16>,
    file_status: Arc<AtomicU16>,
    hits: mpsc::UnboundedReceiver<Hit>,
}

#[derive(Clone)]
struct ReceiverState {
    status: Arc<AtomicU16>,
    file_status: Arc<AtomicU16>,
    hits: mpsc::UnboundedSender<Hit>,
    file: Vec<u8>,
}

async fn receive(State(st): State<ReceiverState>, headers: HeaderMap, raw: String) -> StatusCode {
    let body = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let _ = st.hits.send(Hit { headers, body, raw });
    StatusCode::from_u16(st.status.load(Ordering::SeqCst)).unwrap()
}

async fn file(State(st): State<ReceiverState>) -> (StatusCode, Vec<u8>) {
    let status = StatusCode::from_u16(st.file_status.load(Ordering::SeqCst)).unwrap();
    if status.is_success() {
        (status, st.file)
    } else {
        (status, b"nope".to_vec())
    }
}

async fn receiver(file: Vec<u8>) -> Receiver {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let status = Arc::new(AtomicU16::new(200));
    let file_status = Arc::new(AtomicU16::new(200));
    let (tx, rx) = mpsc::unbounded_channel();
    let router = Router::new()
        .route("/events", post(receive))
        .route("/file", any(self::file))
        .with_state(ReceiverState {
            status: status.clone(),
            file_status: file_status.clone(),
            hits: tx,
            file,
        });
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Receiver {
        url: format!("http://{addr}/events"),
        file_url: format!("http://{addr}/file"),
        status,
        file_status,
        hits: rx,
    }
}

impl Receiver {
    /// Point the configuration at this receiver.
    fn configure(&self, config: &mut Config) {
        config.webhook.url = Some(self.url.clone());
        config.webhook.secret = Some(SECRET.to_string().into());
        config.webhook.allow_insecure = true;
        config.fetch.allow_private_networks = true;
    }

    /// The next delivery, which must have arrived already.
    fn hit(&mut self) -> Hit {
        self.hits.try_recv().expect("a delivery")
    }

    fn quiet(&mut self) {
        assert!(self.hits.try_recv().is_err(), "an unexpected delivery");
    }
}

async fn setup_receiving(file: Vec<u8>) -> (TestApp, Receiver) {
    let clamd = fake_clamd().await;
    let receiver = receiver(file).await;
    let app = setup_tuned(clamd, |c| receiver.configure(c)).await;
    (app, receiver)
}

/// Verify the Standard Webhooks headers and signature of a delivery, as a
/// receiver would, and return its id.
fn verify(hit: &Hit) -> String {
    let header = |name: &str| hit.headers[name].to_str().unwrap().to_string();
    assert_eq!(header("content-type"), "application/json");
    assert!(header("user-agent").starts_with("agrafe/"));
    let id = header("webhook-id");
    let timestamp: i64 = header("webhook-timestamp").parse().unwrap();
    let skew = (Timestamp::now().as_second() - timestamp).abs();
    assert!(skew < 60, "timestamp {timestamp} is {skew}s off");
    let key = STANDARD
        .decode(SECRET.strip_prefix("whsec_").unwrap())
        .unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).unwrap();
    mac.update(format!("{id}.{timestamp}.{}", hit.raw).as_bytes());
    let expected = format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()));
    let signatures = header("webhook-signature");
    assert!(
        signatures.split(' ').any(|s| s == expected),
        "signature {signatures} is not {expected}"
    );
    id
}

/// The one notify job for the blob, with its payload.
async fn notify_job(app: &TestApp, id: &str) -> Option<(i64, Value)> {
    sqlx::query_as::<_, (i64, Value)>(
        "SELECT id, payload FROM jobs WHERE blob_id = $1 AND kind = 'notify'",
    )
    .bind(parse_id(id))
    .fetch_optional(app.state.pool())
    .await
    .unwrap()
}

async fn set_expired(app: &TestApp, id: &str) {
    sqlx::query("UPDATE blobs SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn ready_is_delivered_signed_with_the_metadata() {
    let (app, mut receiver) = setup_receiving(Vec::new()).await;

    // The inline scan: `uploading` to `ready` in the PUT itself.
    let blob = ready_blob(&app, &png_bytes(1024)).await;
    let id = id_of(&blob);
    let (_, payload) = notify_job(&app, id).await.expect("a notify job");
    assert_eq!(payload["event"], "ready");
    assert!(payload["occurred_at"].is_string());

    assert!(run_job(&app, JobKind::Notify).await);
    let hit = receiver.hit();
    assert_eq!(verify(&hit), payload["id"].as_str().unwrap());
    assert_eq!(hit.body["event"], "ready");
    assert_eq!(hit.body["attempt"], 1);
    assert_eq!(hit.body["occurred_at"], payload["occurred_at"]);
    // The same object as GET /blobs/:id.
    let shown = get_blob(&app, id).await;
    assert_eq!(hit.body["blob"]["id"], shown["id"]);
    assert_eq!(hit.body["blob"]["state"], "ready");
    assert_eq!(hit.body["blob"]["urls"], shown["urls"]);
    assert_eq!(hit.body["blob"]["envelope"], shown["envelope"]);
    assert_eq!(hit.body["blob"]["attachments"], json!([]));

    // Delivered: the job is gone, the metric moved.
    assert!(notify_job(&app, id).await.is_none());
    assert!(!run_job(&app, JobKind::Notify).await);
    let (_, _, metrics) = fetch(&app.private, req(Method::GET, "/metrics")).await;
    let metrics = String::from_utf8(metrics).unwrap();
    assert!(
        metrics.contains(r#"agrafe_webhook_deliveries_total{event="ready",result="delivered"} 1"#),
        "{metrics}"
    );
    receiver.quiet();
}

#[tokio::test]
async fn a_deferred_scan_reports_ready_or_infected() {
    let (app, mut receiver) = setup_receiving(Vec::new()).await;

    // Over inline_scan_max_size: stored first, scanned by the job.
    let clean = png_bytes(4096);
    let created = create(&app, blob_for(&clean, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, clean).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert!(notify_job(&app, id).await.is_none(), "nothing to tell yet");
    assert!(run_scan_job(&app).await);
    assert_eq!(notify_job(&app, id).await.unwrap().1["event"], "ready");
    assert!(run_job(&app, JobKind::Notify).await);
    let hit = receiver.hit();
    verify(&hit);
    assert_eq!(hit.body["event"], "ready");
    assert_eq!(hit.body["blob"]["state"], "ready");
    assert_eq!(hit.body["blob"]["scan_state"], "clean");

    let bad = infected_png(4096);
    let created = create(&app, blob_for(&bad, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, _) = put_private(&app, id, bad).await;
    assert_eq!(status, StatusCode::OK);
    assert!(run_scan_job(&app).await);
    assert!(run_job(&app, JobKind::Notify).await);
    let hit = receiver.hit();
    verify(&hit);
    assert_eq!(hit.body["event"], "infected");
    assert_eq!(hit.body["blob"]["state"], "infected");
    assert_eq!(hit.body["blob"]["scan_signature"], "Eicar-Test-Signature");
    receiver.quiet();
}

#[tokio::test]
async fn a_dead_job_reports_failed_and_its_retry_reports_ready() {
    let file = png_bytes(1024);
    let (app, mut receiver) = setup_receiving(file.clone()).await;
    receiver.file_status.store(404, Ordering::SeqCst);

    // A 404 kills the fetch at once: the blob is stamped, and the backend
    // is told.
    let created = create(
        &app,
        json!({
            "filename": "photo.png",
            "content_type": "image/png",
            "input_type": "url",
            "url": receiver.file_url,
            "skip_virus_scan": true
        }),
    )
    .await;
    let id = id_of(&created);
    assert!(run_job(&app, JobKind::Fetch).await);
    let shown = get_blob(&app, id).await;
    assert!(shown["failed_at"].is_string(), "{shown}");
    assert_eq!(notify_job(&app, id).await.unwrap().1["event"], "failed");
    assert!(run_job(&app, JobKind::Notify).await);
    let hit = receiver.hit();
    verify(&hit);
    assert_eq!(hit.body["event"], "failed");
    assert_eq!(hit.body["blob"]["state"], "created");
    assert_eq!(hit.body["blob"]["failed_at"], shown["failed_at"]);
    assert_eq!(hit.body["blob"]["last_error"], shown["last_error"]);

    // The retry, once the file is there, ends in `ready`.
    receiver.file_status.store(200, Ordering::SeqCst);
    let (status, body) = call(
        &app.private,
        req(Method::POST, &format!("/blobs/{id}/retry")),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(run_job(&app, JobKind::Fetch).await);
    assert_eq!(notify_job(&app, id).await.unwrap().1["event"], "ready");
    assert!(run_job(&app, JobKind::Notify).await);
    let hit = receiver.hit();
    verify(&hit);
    assert_eq!(hit.body["event"], "ready");
    assert_eq!(hit.body["blob"]["state"], "ready");
    assert_eq!(hit.body["blob"]["failed_at"], Value::Null);
    receiver.quiet();
}

#[tokio::test]
async fn a_failing_receiver_leaves_a_dead_job_the_retry_revives() {
    let (app, mut receiver) = setup_receiving(Vec::new()).await;
    let pool = app.state.pool();
    receiver.status.store(500, Ordering::SeqCst);

    let blob = ready_blob(&app, &png_bytes(1024)).await;
    let id = id_of(&blob);
    let (job, payload) = notify_job(&app, id).await.unwrap();
    let delivery = payload["id"].as_str().unwrap().to_string();

    // The attempt fails and backs off, the response body in last_error.
    assert!(run_job(&app, JobKind::Notify).await);
    receiver.hit();
    let shown = app.state.find_job(job).await.unwrap().unwrap();
    assert_eq!(app.state.job_state(&shown), jobs::JobState::Scheduled);
    assert!(
        shown
            .last_error
            .as_deref()
            .unwrap()
            .starts_with("webhook: the receiver answered 500"),
        "{:?}",
        shown.last_error
    );
    assert!(!run_job(&app, JobKind::Notify).await, "backing off");

    // The last attempt makes it dead, and the blob says nothing of it: a
    // notification is not what finishes the blob.
    sqlx::query("UPDATE jobs SET run_at = now(), attempts = max_attempts WHERE id = $1")
        .bind(job)
        .execute(pool)
        .await
        .unwrap();
    assert!(run_job(&app, JobKind::Notify).await);
    receiver.hit();
    let (status, body) = call(&app.private, req(Method::GET, "/jobs?state=dead")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["id"], job);
    assert_eq!(body["items"][0]["kind"], "notify");
    assert_eq!(get_blob(&app, id).await["failed_at"], Value::Null);
    let (_, _, metrics) = fetch(&app.private, req(Method::GET, "/metrics")).await;
    let metrics = String::from_utf8(metrics).unwrap();
    assert!(
        metrics.contains(r#"agrafe_webhook_deliveries_total{event="ready",result="failed"} 2"#),
        "{metrics}"
    );

    // The receiver is back; the operator's retry drains it.
    receiver.status.store(200, Ordering::SeqCst);
    let (status, body) = call(
        &app.private,
        req(Method::POST, &format!("/jobs/{job}/retry")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(run_job(&app, JobKind::Notify).await);
    let hit = receiver.hit();
    assert_eq!(verify(&hit), delivery, "the id is the delivery's");
    assert_eq!(hit.body["event"], "ready");
    assert_eq!(hit.body["attempt"], 1, "the attempts started over");
    assert!(notify_job(&app, id).await.is_none());
    receiver.quiet();
}

#[tokio::test]
async fn an_unreachable_receiver_fails_the_attempt() {
    let clamd = fake_clamd().await;
    let closed = closed_port().await;
    let app = setup_tuned(clamd, |c| {
        c.webhook.url = Some(format!("http://{closed}/events"));
        c.webhook.secret = Some(SECRET.to_string().into());
    })
    .await;
    let blob = ready_blob(&app, &png_bytes(1024)).await;
    let id = id_of(&blob);
    assert!(run_job(&app, JobKind::Notify).await);
    let (job, _) = notify_job(&app, id).await.unwrap();
    let error = app
        .state
        .find_job(job)
        .await
        .unwrap()
        .unwrap()
        .last_error
        .unwrap();
    assert!(error.starts_with("webhook: request failed: "), "{error}");
}

#[tokio::test]
async fn the_reaper_reports_expiry() {
    let (app, mut receiver) = setup_receiving(Vec::new()).await;

    // A ready blob nobody attached, and one whose upload never came.
    let ready = ready_blob(&app, &png_bytes(1024)).await;
    assert!(run_job(&app, JobKind::Notify).await);
    receiver.hit();
    let mut never = raw_blob();
    never["expires_in"] = json!("1h");
    let never = create(&app, never).await;
    for blob in [&ready, &never] {
        set_expired(&app, id_of(blob)).await;
    }
    let report = reap::reap(&app.state, Timestamp::now()).await.unwrap();
    assert_eq!(report.expired, 2);

    for blob in [&ready, &never] {
        let id = id_of(blob);
        assert_eq!(notify_job(&app, id).await.unwrap().1["event"], "expired");
    }
    assert!(run_job(&app, JobKind::Notify).await);
    assert!(run_job(&app, JobKind::Notify).await);
    let mut states = Vec::new();
    for _ in 0..2 {
        let hit = receiver.hit();
        verify(&hit);
        assert_eq!(hit.body["event"], "expired");
        assert_eq!(hit.body["blob"]["state"], "expired");
        states.push(hit.body["blob"]["uploaded_at"].is_string());
    }
    states.sort();
    assert_eq!(states, [false, true]);
    receiver.quiet();
}

#[tokio::test]
async fn a_newer_event_replaces_the_pending_one() {
    let (app, mut receiver) = setup_receiving(Vec::new()).await;

    // `ready` undelivered when the blob expires: the backend hears of
    // the expiry, with the state behind it, once.
    let blob = ready_blob(&app, &png_bytes(1024)).await;
    let id = id_of(&blob);
    let (first, payload) = notify_job(&app, id).await.unwrap();
    assert_eq!(payload["event"], "ready");
    set_expired(&app, id).await;
    reap::reap(&app.state, Timestamp::now()).await.unwrap();
    let (job, replaced) = notify_job(&app, id).await.unwrap();
    assert_eq!(job, first, "the same row");
    assert_eq!(replaced["event"], "expired");
    assert_ne!(replaced["id"], payload["id"], "a new delivery id");
    assert_eq!(jobs_for(&app, id).await, ["notify", "purge", "render"]);

    assert!(run_job(&app, JobKind::Notify).await);
    let hit = receiver.hit();
    assert_eq!(hit.body["event"], "expired");
    assert_eq!(hit.body["blob"]["state"], "expired");
    assert!(!run_job(&app, JobKind::Notify).await);
    receiver.quiet();
}

#[tokio::test]
async fn events_can_be_filtered() {
    let clamd = fake_clamd().await;
    let receiver = receiver(Vec::new()).await;
    let app = setup_tuned(clamd, |c| {
        receiver.configure(c);
        c.webhook.events = Some(vec![Event::Infected, Event::Failed]);
    })
    .await;
    let blob = ready_blob(&app, &png_bytes(1024)).await;
    assert_eq!(jobs_for(&app, id_of(&blob)).await, ["render"]);
}

#[tokio::test]
async fn nothing_is_enqueued_without_a_webhook() {
    let app = setup().await;
    assert!(app.state.webhook().is_none());
    let blob = ready_blob(&app, &png_bytes(1024)).await;
    let id = id_of(&blob);
    assert_eq!(jobs_for(&app, id).await, ["render"]);
    set_expired(&app, id).await;
    reap::reap(&app.state, Timestamp::now()).await.unwrap();
    assert_eq!(jobs_for(&app, id).await, ["purge", "render"]);
    assert!(!run_job(&app, JobKind::Notify).await);
}
