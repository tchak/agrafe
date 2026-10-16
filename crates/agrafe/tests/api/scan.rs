//! Virus scanning, inline during the upload and deferred to a job, against
//! a fake clamd that misbehaves on demand.

use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::support::*;

/// Clean: straight to `ready`, scanned, representations queued.
#[tokio::test]
async fn inline_scan_clean() {
    let app = setup().await;
    let bytes = png_bytes(1500);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, body) = call(&app.public, put_raw(&public_url(&created, "upload"), bytes)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "ready");
    assert_eq!(body["scan_state"], "clean");
    let got = get_blob(&app, id).await;
    assert!(got["scanned_at"].is_string());
    assert!(got["scan_signature"].is_null());
    assert_eq!(jobs_for(&app, id).await, vec!["preview", "thumbnail"]);
}

/// Infected: the upload is refused with its own code, yet the bytes are kept
/// and the blob is terminal, so the backend sees what happened.
#[tokio::test]
async fn inline_scan_infected() {
    let app = setup().await;
    let bytes = infected_png(1500);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let upload = public_url(&created, "upload");
    let (status, body) = call(&app.public, put_raw(&upload, bytes.clone())).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "infected");
    assert!(
        !body["error"]["message"].as_str().unwrap().contains("Eicar"),
        "the public port does not say which signature: {body}"
    );
    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "infected");
    assert_eq!(got["scan_state"], "infected");
    assert!(got["scanned_at"].is_string());
    assert_eq!(got["scan_signature"], "Eicar-Test-Signature");
    assert!(jobs_for(&app, id).await.is_empty(), "nothing renders it");
    assert_eq!(
        stored_plaintext(&app, id).await,
        bytes,
        "kept for forensics"
    );

    // Terminal: gone from the public port, never readable on the private one.
    let (status, _) = call(
        &app.public,
        req(Method::GET, &public_url(&created, "metadata")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let uri = format!("/blobs/{id}/original/photo.png");
    let (status, _) = call(&app.private, req(Method::GET, &uri)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(&app.public, put_raw(&upload, bytes)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "write-once holds");
}

/// The private port names the signature; the backend is who needs it.
#[tokio::test]
async fn infected_upload_names_the_signature_privately() {
    let app = setup().await;
    let bytes = infected_png(1500);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "infected");
    let said = body["error"]["message"].as_str().unwrap();
    assert!(said.contains("Eicar-Test-Signature"), "{body}");
    assert_eq!(get_blob(&app, id).await["state"], "infected");
}

/// The cheap checks come first: a wrong checksum is a plain 422 even when the
/// bytes are infected, and nothing is stored.
#[tokio::test]
async fn checksum_is_checked_before_the_verdict() {
    let app = setup().await;
    let bytes = infected_png(1500);
    let mut blob = blob_for(&bytes, "image/png", "raw");
    blob["checksum"] = json!(STANDARD.encode([9u8; 32]));
    let created = create(&app, blob).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "unprocessable");
    assert_eq!(get_blob(&app, id).await["state"], "created");
}

/// Vouched-for bytes never reach clamd, even infected ones.
#[tokio::test]
async fn vouched_bytes_skip_the_scan() {
    let app = setup().await;
    let body = ready_blob(&app, &infected_png(1500)).await;
    assert_eq!(body["scan_state"], "skipped");
}

/// Too large for the inline scan: stored, then scanned by the job.
#[tokio::test]
async fn deferred_scan_clean() {
    let app = setup().await;
    assert!(!run_scan_job(&app).await, "nothing queued yet");

    let bytes = png_bytes(5000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert_eq!(jobs_for(&app, id).await, vec!["scan"]);

    assert!(run_scan_job(&app).await);
    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "ready");
    assert_eq!(got["scan_state"], "clean");
    assert!(got["scanned_at"].is_string());
    assert_eq!(jobs_for(&app, id).await, vec!["preview", "thumbnail"]);
    assert!(!run_scan_job(&app).await, "the scan job is gone");
    let (status, _) = call(&app.public, req(Method::GET, &public_url(&got, "original"))).await;
    assert_eq!(status, StatusCode::OK, "readable once clean");
}

/// Infected: terminal, object kept, nothing rendered.
#[tokio::test]
async fn deferred_scan_infected() {
    let app = setup().await;
    let bytes = infected_png(5000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, _) = put_private(&app, id, bytes.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(run_scan_job(&app).await);
    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "infected");
    assert_eq!(got["scan_state"], "infected");
    assert_eq!(got["scan_signature"], "Eicar-Test-Signature");
    assert!(jobs_for(&app, id).await.is_empty());
    assert_eq!(stored_plaintext(&app, id).await, bytes);
}

/// Deleted before the job ran: the job finds nothing to do and succeeds.
#[tokio::test]
async fn scan_job_skips_a_deleted_blob() {
    let app = setup().await;
    let bytes = png_bytes(5000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    put_private(&app, id, bytes).await;
    delete(&app, id).await;
    assert!(run_scan_job(&app).await);
    assert_eq!(get_blob(&app, id).await["state"], "deleted");
    assert!(jobs_for(&app, id).await.is_empty());
    assert!(!run_scan_job(&app).await);
}

/// Deleted while the job waits for clamd: the verdict lands on nothing, and
/// the blob stays deleted, unscanned and unrendered.
#[tokio::test]
async fn delete_during_scan() {
    let received = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let clamd = fake_clamd_in(FakeClamd::Held {
        received: received.clone(),
        release: release.clone(),
    })
    .await;
    let app = setup_with(clamd).await;

    let bytes = infected_png(5000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");

    let state = app.state.clone();
    let job = tokio::spawn(async move {
        agrafe::jobs::runner::run_one(&state, agrafe::jobs::JobKind::Scan, "test")
            .await
            .unwrap()
    });
    received.notified().await;
    assert_eq!(delete(&app, id).await, StatusCode::NO_CONTENT);
    release.notify_one();
    assert!(job.await.unwrap(), "the job ran");

    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "deleted", "{got}");
    assert_eq!(got["scan_state"], "pending", "{got}");
    assert!(got["scan_signature"].is_null(), "{got}");
    assert!(
        jobs_for(&app, id).await.is_empty(),
        "the scan job is done and nothing renders it"
    );
}

#[tokio::test]
async fn empty_file_deferred_scan() {
    // clamd is down when an empty file arrives, so even that scan is
    // deferred; the job must cope with a file that has no segments.
    let addr = closed_port().await;
    let app = setup_with(addr).await;
    let bytes: Vec<u8> = Vec::new();
    let created = create(&app, blob_for(&bytes, "text/plain", "raw")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert_eq!(jobs_for(&app, id).await, vec!["scan"]);

    // clamd comes back on the same address and the retry goes through.
    fake_clamd_on(TcpListener::bind(addr).await.unwrap(), FakeClamd::Scan);
    assert!(run_scan_job(&app).await);
    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "ready", "{got}");
    assert_eq!(got["scan_state"], "clean");
    assert!(jobs_for(&app, id).await.contains(&"thumbnail".to_string()));
}

#[tokio::test]
async fn inline_scans_are_bounded() {
    let app = setup().await;
    // The one inline slot is taken by an upload still in flight.
    let held = app.state.clamd().begin_inline().await.unwrap();

    let bytes = png_bytes(1500);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded", "deferred rather than queued");
    assert_eq!(jobs_for(&app, id).await, vec!["scan"]);

    // The slot comes back when that session ends.
    drop(held);
    let bytes = png_bytes(1500);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "ready");
}

#[tokio::test]
async fn clamd_stream_limit() {
    let addr = fake_clamd_in(FakeClamd::Limit(1000)).await;
    let app = setup_with(addr).await;

    // Inline: clamd hangs up midway. The upload still succeeds and the scan
    // is deferred, exactly as when clamd is unreachable.
    let bytes = png_bytes(1500);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert_eq!(body["scan_state"], "pending");
    assert_eq!(jobs_for(&app, id).await, vec!["scan"]);

    // The job meets the same limit: a failed attempt, the blob stays
    // unreadable, and the error names clamd.
    assert!(run_scan_job(&app).await);
    assert_eq!(get_blob(&app, id).await["state"], "uploaded");
    let error = last_error(&app, id).await;
    assert!(error.contains("clamd"), "{error}");

    // Five segments, clamd gone after the second: whether the job notices
    // at the next write or only at the verdict, the attempt fails the same
    // way. (Which one it is depends on when the reset reaches this side.)
    let bytes = png_bytes(5000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(run_scan_job(&app).await);
    assert_eq!(get_blob(&app, id).await["state"], "uploaded");
    let error = last_error(&app, id).await;
    assert!(error.contains("clamd"), "{error}");
}

#[tokio::test]
async fn clamd_dies_midway() {
    let addr = fake_clamd_in(FakeClamd::Crash(1000)).await;
    let app = setup_with(addr).await;

    // Inline, the body arriving in pieces: clamd vanishes after the third
    // one without a word. The remaining pieces are dropped on the floor, the
    // upload completes and the scan is deferred.
    let bytes = png_bytes(1500);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, body) = call(
        &app.private,
        put_raw_chunked(&format!("/blobs/{id}"), bytes.clone(), 400),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert_eq!(body["scan_state"], "pending");
    assert_eq!(jobs_for(&app, id).await, vec!["scan"]);
    assert_eq!(stored_plaintext(&app, id).await, bytes, "stored whole");

    // The job meets the same fate: a failed attempt naming clamd, the blob
    // still unreadable.
    assert!(run_scan_job(&app).await);
    assert_eq!(get_blob(&app, id).await["state"], "uploaded");
    let error = last_error(&app, id).await;
    assert!(error.contains("clamd"), "{error}");
}

/// clamd takes the stream and never answers: the timeout defers the inline
/// scan instead of holding the upload, and fails the job's attempt.
#[tokio::test]
async fn clamd_hangs() {
    let clamd = fake_clamd_in(FakeClamd::Hang).await;
    let app = setup_tuned(clamd, |c| c.clamav.timeout = Duration::from_millis(200)).await;

    let bytes = png_bytes(1500);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, body) = tokio::time::timeout(Duration::from_secs(5), put_private(&app, id, bytes))
        .await
        .expect("the upload is not held by clamd");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert_eq!(body["scan_state"], "pending");

    assert!(run_scan_job(&app).await);
    assert_eq!(get_blob(&app, id).await["state"], "uploaded");
    let error = last_error(&app, id).await;
    assert!(error.contains("no answer within"), "{error}");
}

#[tokio::test]
async fn clamd_down() {
    let app = setup_with(closed_port().await).await;
    let pool = app.state.pool();

    // A small file would be scanned inline; with clamd unreachable the
    // upload still succeeds and the scan is deferred to a job.
    let bytes = png_bytes(1500);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert_eq!(body["scan_state"], "pending");
    assert_eq!(jobs_for(&app, id).await, vec!["scan"]);

    // The job fails and is retried later with backoff.
    assert!(run_scan_job(&app).await);
    let (attempts, locked, run_at, error): (i32, Option<String>, jiff_sqlx::Timestamp, String) =
        sqlx::query_as(
            "SELECT attempts, locked_by, run_at, last_error FROM jobs WHERE blob_id = $1",
        )
        .bind(parse_id(id))
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(attempts, 1);
    assert!(locked.is_none(), "unlocked for the retry");
    assert!(run_at.to_jiff() > jiff::Timestamp::now(), "backed off");
    assert!(error.contains("clamd"), "{error}");
    assert_eq!(get_blob(&app, id).await["state"], "uploaded");
    assert!(!run_scan_job(&app).await, "not due yet");

    // Out of attempts: the job stays, dead, and no longer blocks a fresh one.
    sqlx::query("UPDATE jobs SET run_at = now(), max_attempts = 2 WHERE blob_id = $1")
        .bind(parse_id(id))
        .execute(pool)
        .await
        .unwrap();
    assert!(run_scan_job(&app).await);
    let (failed, locked): (Option<jiff_sqlx::Timestamp>, Option<String>) =
        sqlx::query_as("SELECT failed_at, locked_by FROM jobs WHERE blob_id = $1")
            .bind(parse_id(id))
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(failed.is_some());
    assert!(locked.is_some(), "a dead job keeps its lock");
    assert!(!run_scan_job(&app).await, "never claimed again");
    let inserted = agrafe::jobs::enqueue(
        pool,
        agrafe::jobs::JobKind::Scan,
        Some(parse_id(id)),
        json!({}),
        jiff::Timestamp::now(),
    )
    .await
    .unwrap();
    assert!(inserted, "the dead job does not hold the pending slot");
}
