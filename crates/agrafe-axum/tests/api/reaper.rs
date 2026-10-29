//! Expiry, the reaper and the purge: expiries recorded, objects removed
//! once nothing may read them, stale uploads reset, dead jobs and
//! tombstones pruned, and the reaper keeping its own clock.

use agrafe::blob::repo;
use agrafe::jobs::reap::{self, Report};
use agrafe::jobs::{self, JobKind};
use axum::http::{Method, StatusCode};
use jiff::Timestamp;
use serde_json::json;
use std::time::Duration;

use crate::support::*;

/// One reap, as of now.
async fn reap_now(app: &TestApp) -> Report {
    reap::reap(&app.state, Timestamp::now()).await.unwrap()
}

/// Make every scheduled purge due and run them all.
async fn purge_all(app: &TestApp) -> usize {
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'purge'")
        .execute(app.state.pool())
        .await
        .unwrap();
    let mut ran = 0;
    while run_job(app, JobKind::Purge).await {
        ran += 1;
    }
    ran
}

async fn set_expired(app: &TestApp, id: &str) {
    sqlx::query("UPDATE blobs SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
}

/// Seconds until the one purge job of `id` runs.
async fn purge_due_in(app: &TestApp, id: &str) -> f64 {
    sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM run_at - now())::float8 FROM jobs \
         WHERE kind = 'purge' AND blob_id = $1",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn purge_removes_every_object_of_a_deleted_blob() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    while run_job(&app, JobKind::Render).await {}
    let other = ready_blob(&app, &png_bytes(100)).await;
    assert_eq!(
        stored_objects(&app).await.len(),
        4,
        "original, preview, thumbnail, other"
    );

    assert_eq!(delete(&app, id).await, StatusCode::NO_CONTENT);
    // Not before the grace period.
    assert!(!run_job(&app, JobKind::Purge).await);
    assert_eq!(stored_objects(&app).await.len(), 4);

    assert_eq!(purge_all(&app).await, 1);
    let left = stored_objects(&app).await;
    assert_eq!(left.len(), 1);
    assert!(
        left[0]
            .location
            .as_ref()
            .starts_with(&object_key(id_of(&other), "")),
        "{left:?}"
    );
    let body = get_blob(&app, id).await;
    assert_eq!(body["state"], "deleted");
    assert!(body["purged_at"].is_string(), "{body}");
    assert_eq!(
        get_blob(&app, id_of(&other)).await["purged_at"],
        json!(null)
    );
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());

    // Purging again finds nothing and is not an error.
    jobs::enqueue(
        app.state.pool(),
        JobKind::Purge,
        Some(parse_id(id)),
        agrafe::jobs::Priority::Normal,
        json!({}),
        Timestamp::now(),
    )
    .await
    .unwrap();
    assert_eq!(purge_all(&app).await, 1);
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());
}

#[tokio::test]
async fn purge_refuses_a_readable_blob() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_blob(&app, &png_bytes(100)).await;
    let id = id_of(&blob);
    jobs::enqueue(
        app.state.pool(),
        JobKind::Purge,
        Some(parse_id(id)),
        agrafe::jobs::Priority::Normal,
        json!({}),
        Timestamp::now(),
    )
    .await
    .unwrap();

    assert!(run_job(&app, JobKind::Purge).await);
    assert_eq!(stored_objects(&app).await.len(), 1);
    assert_eq!(get_blob(&app, id).await["purged_at"], json!(null));
    let (error, attempts, dead): (String, i32, bool) = sqlx::query_as(
        "SELECT last_error, attempts, failed_at IS NOT NULL FROM jobs \
         WHERE kind = 'purge' AND blob_id = $1",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    assert_eq!(error, "purge: blob is ready, not in a terminal state");
    assert_eq!((attempts, dead), (1, true), "dead at once, not retried");
}

#[tokio::test]
async fn reaper_records_expiry_and_schedules_the_purge() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let stored = ready_blob(&app, &png_bytes(100)).await;
    let empty = create(&app, raw_blob()).await;
    let mut later = raw_blob();
    later["expires_in"] = json!("1h");
    let later = create(&app, later).await;
    set_expired(&app, id_of(&stored)).await;
    set_expired(&app, id_of(&empty)).await;

    let report = reap_now(&app).await;
    assert_eq!(report.expired, 2);
    assert_eq!(report.purges, 1, "only the stored one has bytes to purge");
    assert_eq!(report.reset, 0);
    assert_eq!(get_blob(&app, id_of(&stored)).await["state"], "expired");
    assert_eq!(get_blob(&app, id_of(&empty)).await["state"], "expired");
    assert_eq!(get_blob(&app, id_of(&later)).await["state"], "created");
    assert_eq!(jobs_for(&app, id_of(&empty)).await, Vec::<String>::new());
    // The render job of a ready blob is still there; the purge waits for
    // the grace period, as after a delete.
    assert_eq!(
        jobs_for(&app, id_of(&stored)).await,
        vec!["purge", "render"]
    );
    let due = purge_due_in(&app, id_of(&stored)).await;
    assert!((1700.0..=1800.0).contains(&due), "{due}");

    // Nothing left to do.
    assert_eq!(reap_now(&app).await, Report::default());

    assert_eq!(purge_all(&app).await, 1);
    assert!(stored_objects(&app).await.is_empty());
    let body = get_blob(&app, id_of(&stored)).await;
    assert_eq!(body["state"], "expired");
    assert!(body["purged_at"].is_string());
    assert_eq!(reap_now(&app).await, Report::default());
}

#[tokio::test]
async fn infected_blobs_lose_their_bytes_at_expiry_but_keep_their_state() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = infected_png(1500);
    let blob = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&blob);
    let (status, _) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(get_blob(&app, id).await["state"], "infected");
    assert_eq!(stored_objects(&app).await.len(), 1);

    // Kept for forensics while it has no expiry.
    assert_eq!(reap_now(&app).await, Report::default());

    set_expired(&app, id).await;
    let report = reap_now(&app).await;
    assert_eq!(report.expired, 0, "infected is terminal and stays so");
    assert_eq!(report.purges, 1);
    assert_eq!(get_blob(&app, id).await["state"], "infected");

    assert_eq!(purge_all(&app).await, 1);
    assert!(stored_objects(&app).await.is_empty());
    let body = get_blob(&app, id).await;
    assert_eq!(body["state"], "infected");
    assert!(body["purged_at"].is_string());
    assert_eq!(reap_now(&app).await, Report::default());
}

#[tokio::test]
async fn stale_uploads_go_back_to_created() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let stale = create(&app, raw_blob()).await;
    let fresh = create(&app, raw_blob()).await;
    for (blob, age_secs) in [(&stale, 7200.0), (&fresh, 60.0)] {
        sqlx::query(
            "UPDATE blobs SET state = 'uploading', \
             upload_started_at = now() - make_interval(secs => $2), \
             upload_heartbeat_at = now() - make_interval(secs => $2) WHERE id = $1",
        )
        .bind(parse_id(id_of(blob)))
        .bind(age_secs)
        .execute(app.state.pool())
        .await
        .unwrap();
    }

    let report = reap_now(&app).await;
    assert_eq!(report.reset, 1);
    assert_eq!(get_blob(&app, id_of(&stale)).await["state"], "created");
    assert_eq!(get_blob(&app, id_of(&fresh)).await["state"], "uploading");
    assert_eq!(reap_now(&app).await, Report::default());
}

#[tokio::test]
async fn dead_jobs_are_pruned_and_a_lost_purge_is_scheduled_again() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.jobs.keep_failed = Duration::from_secs(3600)
    })
    .await;
    let blob = create(&app, raw_blob()).await;
    let id = id_of(&blob);
    sqlx::query("UPDATE blobs SET state = 'ready', storage_key = $2 WHERE id = $1")
        .bind(parse_id(id))
        .bind(object_key(id, "original"))
        .execute(app.state.pool())
        .await
        .unwrap();
    assert_eq!(delete(&app, id).await, StatusCode::NO_CONTENT);
    assert_eq!(jobs_for(&app, id).await, vec!["purge"]);

    // The purge died: its row stays, so the reaper leaves the blob alone.
    sqlx::query(
        "UPDATE jobs SET locked_at = now() - interval '30 minutes', locked_by = 'w', \
         failed_at = now() - interval '30 minutes', last_error = 'boom' WHERE blob_id = $1",
    )
    .bind(parse_id(id))
    .execute(app.state.pool())
    .await
    .unwrap();
    assert_eq!(reap_now(&app).await, Report::default());

    // Once it has been around for `keep_failed`, it goes and the purge is
    // scheduled anew.
    sqlx::query("UPDATE jobs SET failed_at = now() - interval '2 hours' WHERE blob_id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    let report = reap_now(&app).await;
    assert_eq!(report.pruned, 1);
    assert_eq!(report.purges, 1);
    let (attempts, failed_at): (i32, Option<jiff_sqlx::Timestamp>) =
        sqlx::query_as("SELECT attempts, failed_at FROM jobs WHERE blob_id = $1")
            .bind(parse_id(id))
            .fetch_one(app.state.pool())
            .await
            .unwrap();
    assert_eq!(attempts, 0);
    assert!(failed_at.is_none());

    // Nothing in the bucket for it: purging is a no-op that records itself.
    assert_eq!(purge_all(&app).await, 1);
    assert!(get_blob(&app, id).await["purged_at"].is_string());
}

#[tokio::test]
async fn the_reap_job_keeps_one_row_pending() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.jobs.reap_interval = Duration::from_secs(300)
    })
    .await;
    let pool = app.state.pool();
    let now = Timestamp::now();
    // What the runner does when it starts, from however many processes.
    assert!(reap::schedule(pool, now).await.unwrap());
    assert!(!reap::schedule(pool, now).await.unwrap());

    assert!(run_job(&app, JobKind::Reap).await);
    let (rows, due_in): (i64, f64) = sqlx::query_as(
        "SELECT count(*), max(EXTRACT(EPOCH FROM run_at - now()))::float8 \
         FROM jobs WHERE kind = 'reap'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(rows, 1, "the run scheduled its successor and left");
    assert!((290.0..=300.0).contains(&due_in), "{due_in}");
    assert!(!run_job(&app, JobKind::Reap).await, "not due yet");
    // A restart meanwhile does not add one.
    assert!(!reap::schedule(pool, Timestamp::now()).await.unwrap());
}

/// A reap that finds another holding its lock does nothing, and the job
/// still schedules its successor and leaves. The lock is the
/// installation's: one taken under another schema does not stop it.
#[tokio::test]
async fn a_reap_overlapping_another_does_nothing() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let pool = app.state.pool();
    let blob = create(&app, raw_blob()).await;
    set_expired(&app, id_of(&blob)).await;

    let mut holder = pool.acquire().await.unwrap();
    assert!(jobs::try_lock(&mut holder, reap::LOCK).await.unwrap());
    let report = reap_now(&app).await;
    assert_eq!(
        report,
        Report {
            skipped: true,
            ..Report::default()
        }
    );
    assert_eq!(get_blob(&app, id_of(&blob)).await["state"], "created");

    // Through the job: skipping is not failing, and the clock keeps going.
    assert!(reap::schedule(pool, Timestamp::now()).await.unwrap());
    assert!(run_job(&app, JobKind::Reap).await);
    let (dead, pending): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE failed_at IS NOT NULL), \
                count(*) FILTER (WHERE failed_at IS NULL AND locked_at IS NULL) \
         FROM jobs WHERE kind = 'reap'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!((dead, pending), (0, 1));
    assert_eq!(get_blob(&app, id_of(&blob)).await["state"], "created");
    jobs::unlock(&mut holder, reap::LOCK).await.unwrap();

    // The same lock taken by another installation, another schema of the
    // same database, is not this one's.
    sqlx::query("SET search_path = elsewhere")
        .execute(&mut *holder)
        .await
        .unwrap();
    assert!(jobs::try_lock(&mut holder, reap::LOCK).await.unwrap());
    let report = reap_now(&app).await;
    assert!(!report.skipped);
    assert_eq!(report.expired, 1);
    assert_eq!(get_blob(&app, id_of(&blob)).await["state"], "expired");
    jobs::unlock(&mut holder, reap::LOCK).await.unwrap();
    sqlx::query("RESET search_path")
        .execute(&mut *holder)
        .await
        .unwrap();
}

/// A reap takes a batch of each kind of work, and one that filled a batch
/// brings the next forward rather than leave the rest for an interval.
#[tokio::test]
async fn the_reaper_drains_a_backlog_in_batches() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.jobs.reap_interval = Duration::from_secs(300)
    })
    .await;
    let pool = app.state.pool();
    sqlx::query(
        "INSERT INTO blobs (id, state, filename, content_type, category, input_type, expires_at) \
         SELECT gen_random_uuid(), 'created', 'a.png', 'image/png', 'image', 'raw', \
                now() - interval '1 second' \
         FROM generate_series(1, $1)",
    )
    .bind(reap::BATCH + 1)
    .execute(pool)
    .await
    .unwrap();
    let count = |state: &'static str| async move {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM blobs WHERE state = $1")
            .bind(state)
            .fetch_one(pool)
            .await
            .unwrap()
    };
    let reap_due_in = || async {
        sqlx::query_scalar::<_, f64>(
            "SELECT EXTRACT(EPOCH FROM run_at - now())::float8 FROM jobs WHERE kind = 'reap'",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    };

    assert!(reap::schedule(pool, Timestamp::now()).await.unwrap());
    assert!(run_job(&app, JobKind::Reap).await);
    assert_eq!(count("expired").await, reap::BATCH);
    assert_eq!(count("created").await, 1);
    let due = reap_due_in().await;
    assert!(due <= 0.0, "the next reap runs now: {due}");

    assert!(run_job(&app, JobKind::Reap).await);
    assert_eq!(count("created").await, 0);
    let due = reap_due_in().await;
    assert!(
        (290.0..=300.0).contains(&due),
        "back to the interval: {due}"
    );
}

/// An upload whose completion failed, and whose hand back failed too,
/// leaves its object and its row `uploading`. The reaper resets the row,
/// and the expiry purges the object: an upload had begun on the blob.
#[tokio::test]
async fn an_unfinished_uploads_object_goes_with_the_purge() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(5000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    // The touches go through; the completion and the hand back, which
    // move the row out of `uploading`, fail.
    fail_blob_updates(&app, "OLD.state = 'uploading' AND NEW.state <> 'uploading'").await;
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(get_blob(&app, id).await["state"], "uploading");
    assert_eq!(stored_objects(&app).await.len(), 1);
    restore_blob_updates(&app).await;

    sqlx::query("UPDATE blobs SET upload_heartbeat_at = now() - interval '1 day' WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    assert_eq!(reap_now(&app).await.reset, 1);
    assert_eq!(get_blob(&app, id).await["state"], "created");
    set_expired(&app, id).await;
    let report = reap_now(&app).await;
    assert_eq!((report.expired, report.purges), (1, 1));
    assert_eq!(purge_all(&app).await, 1);
    assert!(stored_objects(&app).await.is_empty());
}

/// An operator discarding a dead purge does not leave the blob's objects
/// to nobody: the purge is scheduled again, due now, as the reaper does
/// when it prunes one.
#[tokio::test]
async fn discarding_a_dead_purge_schedules_it_again() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_blob(&app, &png_bytes(100)).await;
    let id = id_of(&blob);
    while run_job(&app, JobKind::Render).await {}
    assert_eq!(delete(&app, id).await, StatusCode::NO_CONTENT);
    let job: i64 = sqlx::query_scalar(
        "UPDATE jobs SET locked_at = now(), locked_by = 'w', failed_at = now(), last_error = 'boom' \
         WHERE blob_id = $1 AND kind = 'purge' RETURNING id",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap();

    let (status, body) = call(&app.private, req(Method::DELETE, &format!("/jobs/{job}"))).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (again, due_in, failed): (i64, f64, bool) = sqlx::query_as(
        "SELECT id, EXTRACT(EPOCH FROM run_at - now())::float8, failed_at IS NOT NULL \
         FROM jobs WHERE blob_id = $1 AND kind = 'purge'",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    assert_ne!(again, job);
    assert!(!failed);
    assert!(due_in <= 0.0, "{due_in}");
    assert_eq!(purge_all(&app).await, 1);
    assert!(stored_objects(&app).await.is_empty());
}

/// An upload slower than `stale_after` is not stale: the pipeline touches
/// the row while it runs, and the reaper leaves it alone.
#[tokio::test]
async fn a_slow_upload_is_not_a_stale_one() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| c.upload.stale_after = Duration::from_secs(2)).await;
    let bytes = png_bytes(3000);
    let mut blob = blob_for(&bytes, "image/png", "raw");
    blob["skip_virus_scan"] = json!(true);
    let created = create(&app, blob).await;
    let id = id_of(&created).to_string();

    // The body arrives in two pieces with a long pause between them.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(2);
    let req = axum::http::Request::builder()
        .method(axum::http::Method::PUT)
        .uri(format!("/blobs/{id}"))
        .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
        .body(axum::body::Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap();
    let router = app.private.clone();
    let response = tokio::spawn(async move { call(&router, req).await });
    tx.send(Ok(bytes::Bytes::copy_from_slice(&bytes[..1000])))
        .await
        .unwrap();
    wait_for_state(&app, &id, "uploading").await;
    let heartbeat_at = || async {
        let at: jiff_sqlx::Timestamp =
            sqlx::query_scalar("SELECT upload_heartbeat_at FROM blobs WHERE id = $1")
                .bind(parse_id(&id))
                .fetch_one(app.state.pool())
                .await
                .unwrap();
        at.to_jiff()
    };
    let started = heartbeat_at().await;

    // Well past `stale_after` since the upload began.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(heartbeat_at().await > started, "the row was touched");
    assert_eq!(reap_now(&app).await.reset, 0);
    assert_eq!(get_blob(&app, &id).await["state"], "uploading");

    tx.send(Ok(bytes::Bytes::copy_from_slice(&bytes[1000..])))
        .await
        .unwrap();
    drop(tx);
    let (status, body) = within("the upload", response).await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "ready");
}

/// An upload the reaper gave up on, whose blob a later `PUT` took, stops
/// at its next heartbeat instead of finishing on top of the newcomer: it
/// answers `409`, stores nothing and leaves the row to its new owner.
#[tokio::test]
async fn an_upload_that_lost_its_blob_stops() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| c.upload.stale_after = Duration::from_secs(3)).await;
    let bytes = png_bytes(3000);
    let mut blob = blob_for(&bytes, "image/png", "raw");
    blob["skip_virus_scan"] = json!(true);
    let created = create(&app, blob).await;
    let id = id_of(&created).to_string();

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(2);
    let req = axum::http::Request::builder()
        .method(axum::http::Method::PUT)
        .uri(format!("/blobs/{id}"))
        .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
        .body(axum::body::Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap();
    let router = app.private.clone();
    let stalled = tokio::spawn(async move { call(&router, req).await });
    tx.send(Ok(bytes::Bytes::copy_from_slice(&bytes[..1000])))
        .await
        .unwrap();
    wait_for_state(&app, &id, "uploading").await;

    // What the reaper does to a row it takes for abandoned, then a second
    // upload that takes the blob and finishes.
    sqlx::query("UPDATE blobs SET state = 'created' WHERE id = $1")
        .bind(parse_id(&id))
        .execute(app.state.pool())
        .await
        .unwrap();
    let (status, body) = put_private(&app, &id, bytes.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "ready");

    // The first upload notices, at its next heartbeat or at the latest
    // before it would make its object visible, and gives up.
    let _ = tx
        .send(Ok(bytes::Bytes::copy_from_slice(&bytes[1000..])))
        .await;
    drop(tx);
    let (status, body) = within("the stalled upload", stalled).await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // The newcomer's blob is untouched and its bytes are the ones stored.
    let got = get_blob(&app, &id).await;
    assert_eq!(got["state"], "ready");
    assert_eq!(stored_objects(&app).await.len(), 1);
    assert_eq!(stored_plaintext(&app, &id).await, bytes);
}

/// A terminal row outlives its purge as a tombstone, so the backend can
/// tell a blob it deleted from an id that never existed, and goes after
/// `blobs.keep_purged`, with its attachments. A terminal row with nothing
/// to purge, the direct upload a user walked away from, goes the same way.
#[tokio::test]
async fn tombstones_are_pruned_after_keep_purged_with_their_attachments() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.blobs.keep_purged = Duration::ZERO;
    })
    .await;
    let pool = app.state.pool();

    // Purged after a delete.
    let purged = ready_blob(&app, &png_bytes(100)).await;
    assert_eq!(delete(&app, id_of(&purged)).await, StatusCode::NO_CONTENT);
    assert_eq!(purge_all(&app).await, 1);
    assert!(get_blob(&app, id_of(&purged)).await["purged_at"].is_string());
    // Expired with nothing stored, as the reaper left it.
    let empty = create(&app, raw_blob()).await;
    // Expired while attached: an abandoned source. Its attachment stays
    // until the row goes.
    let mut blob = raw_blob();
    blob["attachment"] = json!({"owner_kind": "expense", "owner_id": "1", "role": "export"});
    let abandoned = create(&app, blob).await;
    for id in [id_of(&empty), id_of(&abandoned)] {
        sqlx::query("UPDATE blobs SET state = 'expired' WHERE id = $1")
            .bind(parse_id(id))
            .execute(pool)
            .await
            .unwrap();
    }
    // Still in use: attached and ready.
    let live = ready_blob(&app, &png_bytes(100)).await;
    let (status, _) = call(
        &app.private,
        json_req(
            Method::POST,
            &format!("/blobs/{}/attachments", id_of(&live)),
            json!({"owner_kind": "expense", "owner_id": "1", "role": "receipt"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // Infected and not yet purged: kept for forensics, until its purge.
    let infected = create(&app, raw_blob()).await;
    sqlx::query("UPDATE blobs SET state = 'infected', storage_key = 'k/1' WHERE id = $1")
        .bind(parse_id(id_of(&infected)))
        .execute(pool)
        .await
        .unwrap();

    // Not yet: as of an hour ago, none of them had been around long enough.
    let report = reap::reap(&app.state, Timestamp::now() - Duration::from_secs(3600))
        .await
        .unwrap();
    assert_eq!(report.tombstones, 0);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM blobs")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(rows, 5);

    let report = reap_now(&app).await;
    assert_eq!(report.tombstones, 3, "{report:?}");
    for gone in [&purged, &empty, &abandoned] {
        let (status, _) = call(
            &app.private,
            req(Method::GET, &format!("/blobs/{}", id_of(gone))),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    assert_eq!(get_blob(&app, id_of(&live)).await["state"], "ready");
    assert_eq!(get_blob(&app, id_of(&infected)).await["state"], "infected");
    let (_, attachments) = call(&app.private, req(Method::GET, "/attachments")).await;
    let items = attachments["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{attachments}");
    assert_eq!(items[0]["blob_id"], id_of(&live));
    assert_eq!(reap_now(&app).await.tombstones, 0);
}

/// An attachment at creation keeps a blob from expiring, but not from
/// being abandoned: a `created` blob nothing has touched for
/// `upload.abandoned_after` is expired, attachments kept, like a source
/// nobody finished. An unattached one keeps to its own `expires_at`.
#[tokio::test]
async fn an_attached_blob_nothing_reached_is_expired_as_abandoned() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let pool = app.state.pool();
    let attachment = json!({"owner_kind": "expense", "owner_id": "1", "role": "receipt"});

    let mut blob = raw_blob();
    blob["attachment"] = attachment.clone();
    let untouched = create(&app, blob).await;
    let mut blob = raw_blob();
    blob["attachment"] = attachment.clone();
    let touched = create(&app, blob).await;
    let mut blob = raw_blob();
    blob["expires_in"] = json!("7d");
    let unattached = create(&app, blob).await;
    let bytes = png_bytes(100);
    let mut blob = blob_for(&bytes, "image/png", "raw");
    blob["skip_virus_scan"] = json!(true);
    blob["attachment"] = attachment;
    let uploaded = create(&app, blob).await;
    let (status, body) = put_private(&app, id_of(&uploaded), bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // All of them were last touched two days ago, as far as the row says,
    // except the touched one, which got a failed upload an hour ago. The
    // trigger that keeps `updated_at` current has to step aside for that.
    sqlx::query("ALTER TABLE blobs DISABLE TRIGGER blobs_updated_at")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE blobs SET updated_at = now() - interval '2 days'")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE blobs SET updated_at = now() - interval '1 hour' WHERE id = $1")
        .bind(parse_id(id_of(&touched)))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE blobs ENABLE TRIGGER blobs_updated_at")
        .execute(pool)
        .await
        .unwrap();

    let report = reap_now(&app).await;
    assert_eq!(report.abandoned, 1, "{report:?}");
    assert_eq!(report.expired, 0, "{report:?}");
    assert_eq!(get_blob(&app, id_of(&untouched)).await["state"], "expired");
    assert_eq!(get_blob(&app, id_of(&touched)).await["state"], "created");
    assert_eq!(get_blob(&app, id_of(&unattached)).await["state"], "created");
    assert_eq!(get_blob(&app, id_of(&uploaded)).await["state"], "ready");

    // The owner's side learns of it from the listing.
    let (status, body) = call(
        &app.private,
        req(
            Method::GET,
            &format!("/attachments?blob_id={}", id_of(&untouched)),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["blob_state"], "expired");
}

/// The reaper's statements walk their partial indexes even under the
/// generic plan a pooled connection may settle on, built without the
/// parameters' values: what the planner must prove to use a partial index
/// is spelled into the SQL. Each batch is read off its index in order,
/// with no sort, so the limit stops the walk. Sequential and bitmap scans
/// are off so that, on a table this small, the planner shows the index it
/// would walk, and falls back to a scan only when it cannot use one.
#[tokio::test]
async fn the_reaper_walks_its_indexes_under_a_generic_plan() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let mut conn = app.state.pool().acquire().await.unwrap();
    for off in ["SET enable_seqscan = off", "SET enable_bitmapscan = off"] {
        sqlx::query(off).execute(&mut *conn).await.unwrap();
    }
    let plans = repo::explain_reaper(&mut conn).await.unwrap();
    let want: &[(&str, &[&str])] = &[
        ("expire", &["blobs_expires_at_idx"]),
        (
            "expire_abandoned",
            &[
                "blobs_stale_source_idx",
                "blobs_abandoned_idx",
                "blobs_dead_fetch_idx",
            ],
        ),
        ("infected_due_for_purge", &["blobs_infected_unpurged_idx"]),
        (
            "prune_tombstones",
            &["blobs_purged_at_idx", "blobs_empty_tombstone_idx"],
        ),
        ("reset_stale_uploads", &["blobs_uploading_idx"]),
    ];
    assert_eq!(plans.len(), want.len());
    for ((name, plan), (want_name, indexes)) in plans.iter().zip(want) {
        assert_eq!(name, want_name);
        assert!(!plan.contains("Seq Scan"), "{name}: {plan}");
        assert!(!plan.contains("Sort"), "{name}: {plan}");
        for index in *indexes {
            assert!(plan.contains(index), "{name}: {plan}");
        }
    }
}

/// A reap that fails is superseded by the successor it scheduled before
/// working, through the runner's transaction: not left locked until its
/// lease lapses, nor retried with backoff.
#[tokio::test]
async fn a_failing_reap_is_superseded_by_its_successor() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let pool = app.state.pool();
    let blob = ready_blob(&app, &png_bytes(100)).await;
    set_expired(&app, id_of(&blob)).await;
    sqlx::query("DELETE FROM jobs WHERE kind = 'reap'")
        .execute(pool)
        .await
        .unwrap();
    reap::schedule(pool, Timestamp::now()).await.unwrap();
    fail_blob_updates(&app, "true").await;
    assert!(run_job(&app, JobKind::Reap).await);
    restore_blob_updates(&app).await;

    let reaps: Vec<(Option<String>, bool, bool)> = sqlx::query_as(
        "SELECT locked_by, failed_at IS NOT NULL, run_at > now() FROM jobs WHERE kind = 'reap'",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(reaps, vec![(None, false, true)], "only the successor");
}
