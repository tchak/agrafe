//! The job queue and the workers that drain it.

use axum::http::StatusCode;
use futures::FutureExt as _;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

use crate::support::*;

/// The workers proper: an enqueue wakes them long before the poll interval,
/// and shutdown stops them.
#[tokio::test]
async fn workers_wake_on_notify_and_stop_on_shutdown() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.jobs.poll_interval = Duration::from_secs(3600);
        c.database.max_connections = 8;
        // This process scans; another one would render.
        c.jobs.kinds = Some(vec![agrafe::jobs::JobKind::Scan]);
    })
    .await;

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let shutdown = async move {
        let _ = stopped.await;
    }
    .boxed()
    .shared();
    let workers = tokio::spawn(agrafe::jobs::runner::run(app.state.clone(), shutdown));

    // Wait for the listener, or the notify could go out before it listens.
    let mut listening = false;
    for _ in 0..100 {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND query LIKE 'LISTEN%'",
        )
        .fetch_one(app.state.pool())
        .await
        .unwrap();
        if n > 0 {
            listening = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(listening, "the job listener never started");

    let bytes = png_bytes(5000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    wait_for_state(&app, id, "ready").await;
    assert_eq!(
        jobs_for(&app, id).await,
        ["render"],
        "left for a render worker"
    );

    // The listener's connection dies: it comes back, and the next enqueue
    // still wakes a worker an hour before the poll would.
    let killed: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = current_database() AND query LIKE 'LISTEN%'",
    )
    .fetch_all(app.state.pool())
    .await
    .unwrap();
    assert_eq!(killed, [true]);
    let bytes = png_bytes(5001);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let again = id_of(&created);
    // Enqueued only once the listener is back, or the notify is lost.
    let mut back = false;
    for _ in 0..200 {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND query LIKE 'LISTEN%'",
        )
        .fetch_one(app.state.pool())
        .await
        .unwrap();
        if n > 0 {
            back = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(back, "the job listener never came back");
    let (status, body) = put_private(&app, again, bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_for_state(&app, again, "ready").await;

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), workers)
        .await
        .expect("the workers stop")
        .unwrap()
        .unwrap();
}

/// Shutdown stops the claims however much is due: the workers return
/// without draining their backlog, which waits for the next process.
#[tokio::test]
async fn shutdown_stops_the_claims_with_a_backlog() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.jobs.kinds = Some(vec![agrafe::jobs::JobKind::Purge]);
    })
    .await;
    // Purges of deleted blobs, each one done at once.
    for n in 0..20 {
        let blob = ready_blob(&app, &png_bytes(100 + n)).await;
        assert_eq!(delete(&app, id_of(&blob)).await, StatusCode::NO_CONTENT);
    }
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'purge'")
        .execute(app.state.pool())
        .await
        .unwrap();
    // The signal came before the workers started.
    tokio::time::timeout(
        Duration::from_secs(5),
        agrafe::jobs::runner::run(app.state.clone(), std::future::ready(())),
    )
    .await
    .expect("the workers stop")
    .unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'purge'")
        .fetch_one(app.state.pool())
        .await
        .unwrap();
    assert_eq!(left, 20, "nothing claimed after the signal");
}

/// A job running longer than its lease keeps it: the heartbeat renews it,
/// so no other worker claims the job meanwhile.
#[tokio::test]
async fn running_job_keeps_its_lease() {
    let db = TestDatabase::new().await;
    let received = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let clamd = fake_clamd_in(FakeClamd::Held {
        received: received.clone(),
        release: release.clone(),
    })
    .await;
    // The heartbeat beats every second at the least.
    let lease = Duration::from_millis(1200);
    let app = setup_tuned(db, clamd, |c| c.jobs.lease_timeout = lease).await;

    let bytes = png_bytes(5000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let state = app.state.clone();
    let job = tokio::spawn(async move {
        agrafe::jobs::runner::run_one(&state, agrafe::jobs::JobKind::Scan, "w1")
            .await
            .unwrap()
    });
    within("clamd to receive the stream", received.notified()).await;
    let locked_at = || async {
        let at: jiff_sqlx::Timestamp =
            sqlx::query_scalar("SELECT locked_at FROM jobs WHERE kind = 'scan'")
                .fetch_one(app.state.pool())
                .await
                .unwrap();
        at.to_jiff()
    };
    let claimed = locked_at().await;

    // Past the lease, with the job still waiting on clamd.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(locked_at().await > claimed, "the lease was renewed");
    let stolen = agrafe::jobs::claim(app.state.pool(), agrafe::jobs::JobKind::Scan, "w2", lease)
        .await
        .unwrap();
    assert!(stolen.is_none(), "another worker took the job: {stolen:?}");

    release.notify_one();
    assert!(within("the scan job", job).await.unwrap());
    assert_eq!(get_blob(&app, id).await["state"], "ready");
}

/// A lease lost all the same, another worker holding the job after a
/// lapse, ends the attempt where it stands: the job is the other worker's
/// to finish, and two runs of one job would write the same objects.
#[tokio::test]
async fn a_lost_lease_ends_the_attempt() {
    let db = TestDatabase::new().await;
    let received = Arc::new(Notify::new());
    let clamd = fake_clamd_in(FakeClamd::Held {
        received: received.clone(),
        release: Arc::new(Notify::new()),
    })
    .await;
    let lease = Duration::from_millis(1200);
    let app = setup_tuned(db, clamd, |c| c.jobs.lease_timeout = lease).await;
    let bytes = png_bytes(5000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let (status, body) = put_private(&app, id_of(&created), bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let state = app.state.clone();
    let job = tokio::spawn(async move {
        agrafe::jobs::runner::run_one(&state, agrafe::jobs::JobKind::Scan, "w1")
            .await
            .unwrap()
    });
    within("clamd to receive the stream", received.notified()).await;
    // Reclaimed by another worker while clamd holds the scan for ever.
    sqlx::query("UPDATE jobs SET locked_by = 'w2', locked_at = now() WHERE kind = 'scan'")
        .execute(app.state.pool())
        .await
        .unwrap();
    assert!(within("the abandoned attempt", job).await.unwrap());
    let held_by: Vec<Option<String>> =
        sqlx::query_scalar("SELECT locked_by FROM jobs WHERE kind = 'scan'")
            .fetch_all(app.state.pool())
            .await
            .unwrap();
    assert_eq!(held_by, vec![Some("w2".to_owned())], "left to w2");
}

/// A lease lost between the last renewal and the attempt's end: whatever
/// the first holder makes of the attempt, the row is the new holder's,
/// left as it is, and the blob takes no verdict from the old attempt.
#[tokio::test]
async fn a_lost_lease_leaves_the_row_to_its_new_holder() {
    let db = TestDatabase::new().await;
    use agrafe::jobs::{self, Failure, JobKind, Priority};

    let app = setup(db).await;
    let webhook = app.state.config().webhook.clone();
    let pool = app.state.pool();
    let now = jiff::Timestamp::now();
    let lease = Duration::from_secs(600);
    let created = create(&app, raw_blob()).await;
    let id = id_of(&created);
    let blob = parse_id(id);
    assert!(
        jobs::enqueue(
            pool,
            JobKind::Scan,
            Some(blob),
            Priority::Normal,
            json!({}),
            now
        )
        .await
        .unwrap()
    );
    let first = jobs::claim(pool, JobKind::Scan, "w1", lease)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE jobs SET locked_at = now() - interval '11 minutes' WHERE id = $1")
        .bind(first.id)
        .execute(pool)
        .await
        .unwrap();
    let second = jobs::claim(pool, JobKind::Scan, "w2", lease)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.id, first.id);

    let held = || async {
        sqlx::query_as::<_, (Option<String>, i32, bool)>(
            "SELECT locked_by, attempts, failed_at IS NOT NULL FROM jobs WHERE id = $1",
        )
        .bind(first.id)
        .fetch_one(pool)
        .await
        .unwrap()
    };
    let mut conn = pool.acquire().await.unwrap();
    assert!(!jobs::succeed(&mut conn, &first).await.unwrap());
    for permanent in [false, true] {
        let mut tx = pool.begin().await.unwrap();
        assert_eq!(
            jobs::fail(&mut tx, &first, "boom", permanent, None, &webhook, now)
                .await
                .unwrap(),
            Failure::Lost,
            "permanent: {permanent}"
        );
        tx.commit().await.unwrap();
        assert_eq!(held().await, (Some("w2".to_owned()), 2, false));
    }
    // Superseded or not, the row is not the old holder's to remove.
    assert!(
        jobs::enqueue(
            pool,
            JobKind::Scan,
            Some(blob),
            Priority::Normal,
            json!({}),
            now
        )
        .await
        .unwrap()
    );
    assert_eq!(
        jobs::fail(&mut conn, &first, "boom", false, None, &webhook, now)
            .await
            .unwrap(),
        Failure::Lost
    );
    assert_eq!(held().await, (Some("w2".to_owned()), 2, false));
    let got = get_blob(&app, id).await;
    assert!(got["failed_at"].is_null(), "{got}");

    assert!(jobs::succeed(&mut conn, &second).await.unwrap());
    assert_eq!(jobs_for(&app, id).await, vec!["scan"], "the twin is left");
}

#[tokio::test]
async fn job_queue_mechanics() {
    let db = TestDatabase::new().await;
    use agrafe::jobs::{self, Failure, JobKind, Priority};

    let app = setup(db).await;
    let webhook = app.state.config().webhook.clone();
    let pool = app.state.pool();
    let now = jiff::Timestamp::now();
    let lease = Duration::from_secs(600);
    let created = create(&app, raw_blob()).await;
    let blob = parse_id(id_of(&created));

    // One pending job per kind and blob; blob-less jobs count as one blob.
    assert!(
        jobs::enqueue(
            pool,
            JobKind::Scan,
            Some(blob),
            Priority::Normal,
            json!({}),
            now
        )
        .await
        .unwrap()
    );
    assert!(
        !jobs::enqueue(
            pool,
            JobKind::Scan,
            Some(blob),
            Priority::Normal,
            json!({}),
            now
        )
        .await
        .unwrap()
    );
    assert!(
        jobs::enqueue(pool, JobKind::Purge, None, Priority::Normal, json!({}), now)
            .await
            .unwrap()
    );
    assert!(
        !jobs::enqueue(pool, JobKind::Purge, None, Priority::Normal, json!({}), now)
            .await
            .unwrap()
    );

    // Claiming locks; a locked job is invisible until its lease expires.
    let job = jobs::claim(pool, JobKind::Scan, "w1", lease)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.kind, JobKind::Scan);
    assert_eq!(job.attempts, 1);
    assert_eq!(job.locked_by.as_deref(), Some("w1"));
    assert!(
        jobs::claim(pool, JobKind::Scan, "w2", lease)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE jobs SET locked_at = now() - interval '11 minutes' WHERE id = $1")
        .bind(job.id)
        .execute(pool)
        .await
        .unwrap();
    let again = jobs::claim(pool, JobKind::Scan, "w2", lease)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.id, job.id);
    assert_eq!(again.attempts, 2);
    assert_eq!(again.locked_by.as_deref(), Some("w2"));

    // The holder renews its lease; nobody else can, and a renewed lease
    // keeps the job out of reach.
    sqlx::query("UPDATE jobs SET locked_at = now() - interval '11 minutes' WHERE id = $1")
        .bind(job.id)
        .execute(pool)
        .await
        .unwrap();
    assert!(!jobs::touch(pool, job.id, "w1").await.unwrap());
    assert!(jobs::touch(pool, job.id, "w2").await.unwrap());
    assert!(
        jobs::claim(pool, JobKind::Scan, "w1", lease)
            .await
            .unwrap()
            .is_none()
    );

    // Not due: a future job is not claimed; kinds are filtered.
    assert!(
        jobs::enqueue(
            pool,
            JobKind::Render,
            Some(blob),
            Priority::Normal,
            json!({}),
            now + Duration::from_secs(3600)
        )
        .await
        .unwrap()
    );
    assert!(
        jobs::claim(pool, JobKind::Render, "w1", lease)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        jobs::claim(pool, JobKind::Reap, "w1", lease)
            .await
            .unwrap()
            .is_none()
    );

    // Failing unlocks with backoff; the next retry needs the lease to pass or
    // the backoff to elapse.
    let mut conn = pool.acquire().await.unwrap();
    let Failure::Retry(at) = jobs::fail(&mut conn, &again, "boom", false, None, &webhook, now)
        .await
        .unwrap()
    else {
        panic!("attempts remain");
    };
    assert!(at > now);
    assert!(
        jobs::claim(pool, JobKind::Scan, "w1", lease)
            .await
            .unwrap()
            .is_none()
    );

    // A failing job whose twin was enqueued meanwhile steps aside.
    sqlx::query("UPDATE jobs SET run_at = now() WHERE id = $1")
        .bind(job.id)
        .execute(pool)
        .await
        .unwrap();
    let held = jobs::claim(pool, JobKind::Scan, "w1", lease)
        .await
        .unwrap()
        .unwrap();
    assert!(
        jobs::enqueue(
            pool,
            JobKind::Scan,
            Some(blob),
            Priority::Normal,
            json!({}),
            now
        )
        .await
        .unwrap()
    );
    // Inside a transaction, as the runner calls it.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        jobs::fail(&mut tx, &held, "boom", false, None, &webhook, now)
            .await
            .unwrap(),
        Failure::Superseded
    );
    tx.commit().await.unwrap();
    let scans: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'scan'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(scans, 1);

    // So does one whose twin is running already.
    let twin = jobs::claim(pool, JobKind::Scan, "w2", lease)
        .await
        .unwrap()
        .unwrap();
    assert!(
        jobs::enqueue(
            pool,
            JobKind::Scan,
            Some(blob),
            Priority::Normal,
            json!({}),
            now
        )
        .await
        .unwrap()
    );
    let newer = jobs::claim(pool, JobKind::Scan, "w3", lease)
        .await
        .unwrap()
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        jobs::fail(&mut tx, &twin, "boom", false, None, &webhook, now)
            .await
            .unwrap(),
        Failure::Superseded
    );
    tx.commit().await.unwrap();
    let held_by: Vec<Option<String>> =
        sqlx::query_scalar("SELECT locked_by FROM jobs WHERE kind = 'scan'")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(held_by, vec![Some("w3".to_owned())]);
    jobs::succeed(&mut conn, &newer).await.unwrap();

    // Success removes the row.
    let purge = jobs::claim(pool, JobKind::Purge, "w1", lease)
        .await
        .unwrap()
        .unwrap();
    jobs::succeed(&mut conn, &purge).await.unwrap();
    let purges: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'purge'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(purges, 0);
}

/// A dead job stays for an operator to look at, who can then give it its
/// attempts back or throw it away; a live job is out of their reach.
#[tokio::test]
async fn operators_see_retry_and_discard_dead_jobs() {
    let db = TestDatabase::new().await;
    use agrafe::jobs::{self, JobKind, Priority};
    use axum::http::Method;

    let app = setup(db).await;
    let pool = app.state.pool();
    let now = jiff::Timestamp::now();
    let created = create(&app, raw_blob()).await;
    let blob = parse_id(id_of(&created));

    jobs::enqueue(
        pool,
        JobKind::Scan,
        Some(blob),
        Priority::Normal,
        json!({}),
        now,
    )
    .await
    .unwrap();
    jobs::enqueue(pool, JobKind::Purge, None, Priority::Normal, json!({}), now)
        .await
        .unwrap();
    let dead: i64 = sqlx::query_scalar(
        "UPDATE jobs SET failed_at = now(), locked_at = now(), locked_by = 'w', \
                         attempts = 5, last_error = 'boom' \
         WHERE kind = 'scan' RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let pending: i64 = sqlx::query_scalar("SELECT id FROM jobs WHERE kind = 'purge'")
        .fetch_one(pool)
        .await
        .unwrap();

    // Listing by state; the state is derived, not stored.
    let (status, body) = call(&app.private, req(Method::GET, "/jobs?state=dead")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], dead);
    assert_eq!(body["items"][0]["kind"], "scan");
    assert_eq!(body["items"][0]["state"], "dead");
    assert_eq!(body["items"][0]["last_error"], "boom");
    assert_eq!(body["items"][0]["blob_id"], blob.to_string());
    assert_eq!(body["next_cursor"], serde_json::Value::Null);
    let (_, body) = call(&app.private, req(Method::GET, "/jobs?state=due")).await;
    assert_eq!(body["items"][0]["id"], pending);
    assert_eq!(body["items"][0]["state"], "due");
    let (_, body) = call(&app.private, req(Method::GET, "/jobs?kind=scan")).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    let (_, body) = call(&app.private, req(Method::GET, "/jobs?limit=1")).await;
    assert_eq!(body["items"][0]["id"], pending, "newest first");
    assert_eq!(body["next_cursor"], pending);
    let (_, body) = call(
        &app.private,
        req(Method::GET, &format!("/jobs?limit=1&cursor={pending}")),
    )
    .await;
    assert_eq!(body["items"][0]["id"], dead);
    let (status, body) = call(&app.private, req(Method::GET, "/jobs?state=bogus")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = call(&app.private, req(Method::GET, "/jobs?limit=0")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = call(&app.private, req(Method::GET, &format!("/jobs/{dead}"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["attempts"], 5);
    let (status, _) = call(&app.private, req(Method::GET, "/jobs/999999")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A pending job is nobody's to retry or discard.
    let (status, body) = call(
        &app.private,
        req(Method::POST, &format!("/jobs/{pending}/retry")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "conflict");
    let (status, _) = call(
        &app.private,
        req(Method::DELETE, &format!("/jobs/{pending}")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // The blob only records the death of a job that was to finish it; the
    // purge is housekeeping.
    sqlx::query("UPDATE blobs SET failed_at = now(), last_error = 'boom' WHERE id = $1")
        .bind(blob)
        .execute(pool)
        .await
        .unwrap();
    let dead_purge: i64 = sqlx::query_scalar(
        "INSERT INTO jobs (kind, blob_id, failed_at, locked_at, locked_by, last_error) \
         VALUES ('purge', $1, now(), now(), 'w', 'bust') RETURNING id",
    )
    .bind(blob)
    .fetch_one(pool)
    .await
    .unwrap();
    let (status, _) = call(
        &app.private,
        req(Method::POST, &format!("/jobs/{dead_purge}/retry")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let got = get_blob(&app, id_of(&created)).await;
    assert_eq!(
        got["last_error"], "boom",
        "a purge's retry says nothing of the blob"
    );

    // Retrying makes the dead job due again, with a fresh attempt budget,
    // and clears the verdict on the blob.
    let (status, body) = call(
        &app.private,
        req(Method::POST, &format!("/jobs/{dead}/retry")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "due");
    assert_eq!(body["attempts"], 0);
    assert_eq!(body["failed_at"], serde_json::Value::Null);
    assert_eq!(body["last_error"], "boom", "kept until the next attempt");
    let claimed = jobs::claim(
        pool,
        JobKind::Scan,
        "w1",
        std::time::Duration::from_secs(600),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(claimed.id, dead);

    // A dead job whose blob got a newer pending twin has nothing to add.
    sqlx::query("UPDATE jobs SET failed_at = now(), locked_at = now() WHERE id = $1")
        .bind(dead)
        .execute(pool)
        .await
        .unwrap();
    assert!(
        jobs::enqueue(
            pool,
            JobKind::Scan,
            Some(blob),
            Priority::Normal,
            json!({}),
            now
        )
        .await
        .unwrap()
    );
    let (status, body) = call(
        &app.private,
        req(Method::POST, &format!("/jobs/{dead}/retry")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already pending")
    );

    // Discarding is for dead jobs, once.
    let (status, _) = call(&app.private, req(Method::DELETE, &format!("/jobs/{dead}"))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app.private, req(Method::DELETE, &format!("/jobs/{dead}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&app.private, req(Method::GET, &format!("/jobs/{dead}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The queue gauges come from the table, the run counters from the process
/// that ran the job; both show on the private port.
#[tokio::test]
async fn metrics_report_queue_depth_and_runs() {
    let db = TestDatabase::new().await;
    use axum::http::Method;

    let app = setup(db).await;
    let scrape = || async {
        let (status, body) = call(&app.private, req(Method::GET, "/metrics")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body.as_str().unwrap().to_string()
    };

    let text = scrape().await;
    assert!(
        text.contains("agrafe_jobs{kind=\"scan\",priority=\"normal\",state=\"due\"} 0"),
        "{text}"
    );
    assert!(
        text.contains("agrafe_job_runs_total{kind=\"scan\",outcome=\"ok\"} 0"),
        "{text}"
    );
    assert!(text.contains("agrafe_jobs_census_ok 1"), "{text}");

    // Too big for an inline scan: the scan is a due job.
    let bytes = png_bytes(5000);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let (status, body) = put_private(&app, id_of(&created), bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let text = scrape().await;
    assert!(
        text.contains("agrafe_jobs{kind=\"scan\",priority=\"normal\",state=\"due\"} 1"),
        "{text}"
    );
    let oldest = text
        .lines()
        .find(|l| {
            l.starts_with("agrafe_jobs_oldest_due_seconds{kind=\"scan\",priority=\"normal\"}")
        })
        .unwrap_or_else(|| panic!("{text}"));
    let age: f64 = oldest.rsplit(' ').next().unwrap().parse().unwrap();
    assert!(age >= 0.0, "{oldest}");

    assert!(run_scan_job(&app).await);
    let text = scrape().await;
    assert!(
        text.contains("agrafe_jobs{kind=\"scan\",priority=\"normal\",state=\"due\"} 0"),
        "{text}"
    );
    assert!(
        text.contains("agrafe_jobs{kind=\"render\",priority=\"normal\",state=\"due\"} 1"),
        "{text}"
    );
    assert!(
        text.contains("agrafe_job_runs_total{kind=\"scan\",outcome=\"ok\"} 1"),
        "{text}"
    );
    assert!(
        text.contains("agrafe_job_duration_seconds_count{kind=\"scan\"} 1"),
        "{text}"
    );
}

/// With no connection to be had, `/metrics` still answers within the
/// census's timeout, with what the process knows of itself and the census
/// flagged as missing.
#[tokio::test]
async fn metrics_answer_without_the_database() {
    use axum::http::Method;

    let db = TestDatabase::new().await;
    let app = setup(db).await;
    app.state.pool().close().await;
    let started = std::time::Instant::now();
    let (status, body) = call(&app.private, req(Method::GET, "/metrics")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(started.elapsed() < Duration::from_secs(10));
    let text = body.as_str().unwrap();
    assert!(text.contains("agrafe_jobs_census_ok 0"), "{text}");
    assert!(
        text.contains("agrafe_job_runs_total{kind=\"scan\",outcome=\"ok\"} 0"),
        "{text}"
    );
}

/// Within a kind, the highest priority goes first and age breaks ties; a
/// blob's priority travels onto its jobs.
#[tokio::test]
async fn priority_orders_a_kind_and_comes_from_the_blob() {
    let db = TestDatabase::new().await;
    use agrafe::jobs::{self, JobKind, Priority};
    use axum::http::Method;

    let app = setup(db).await;
    let pool = app.state.pool();
    let lease = Duration::from_secs(600);
    let now = jiff::Timestamp::now();

    let mut ids = Vec::new();
    for (priority, name) in [
        (Priority::Low, "low"),
        (Priority::Normal, "first-normal"),
        (Priority::High, "high"),
        (Priority::Normal, "second-normal"),
    ] {
        let created = create(&app, raw_blob()).await;
        let id = parse_id(id_of(&created));
        jobs::enqueue(pool, JobKind::Scan, Some(id), priority, json!({}), now)
            .await
            .unwrap();
        ids.push((name, id));
    }
    let mut order = Vec::new();
    while let Some(job) = jobs::claim(pool, JobKind::Scan, "w", lease).await.unwrap() {
        let name = ids
            .iter()
            .find(|(_, id)| Some(*id) == job.blob_id)
            .unwrap()
            .0;
        order.push((name, job.priority));
    }
    assert_eq!(
        order,
        [
            ("high", Priority::High),
            ("first-normal", Priority::Normal),
            ("second-normal", Priority::Normal),
            ("low", Priority::Low),
        ]
    );

    let (_, body) = call(&app.private, req(Method::GET, "/jobs?priority=high")).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["priority"], "high");

    // Created with a priority, the blob's scan job carries it.
    let mut blob = blob_for(&png_bytes(5000), "image/png", "raw");
    blob["priority"] = json!("high");
    let created = create(&app, blob).await;
    assert_eq!(created["priority"], "high");
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, png_bytes(5000)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = call(
        &app.private,
        req(Method::GET, &format!("/jobs?blob_id={id}&kind=scan")),
    )
    .await;
    assert_eq!(body["items"][0]["priority"], "high");

    let mut blob = raw_blob();
    blob["priority"] = json!("urgent");
    let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", blob)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (_, created) = call(&app.private, json_req(Method::POST, "/blobs", raw_blob())).await;
    assert_eq!(created["priority"], "normal", "the default");
}

/// A handler that panics fails the attempt, not the worker: the job is
/// retried with the panic as its error, and the next attempt runs to the
/// end.
#[tokio::test]
async fn a_panicking_handler_fails_the_attempt() {
    let db = TestDatabase::new().await;
    let app = setup_with(db, closed_port().await).await;
    let bytes = png_bytes(5000);
    let mut blob = blob_for(&bytes, "image/png", "raw");
    blob["skip_virus_scan"] = json!(true);
    let created = create(&app, blob).await;
    let id = id_of(&created);
    let (status, body) = put_private(&app, id, bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(jobs_for(&app, id).await, ["render"]);

    app.gate.panic(At::Get, &object_key(id, "original"));
    assert!(run_job(&app, agrafe::jobs::JobKind::Render).await);
    let (attempts, dead, error): (i32, bool, String) = sqlx::query_as(
        "SELECT attempts, failed_at IS NOT NULL, last_error FROM jobs WHERE blob_id = $1",
    )
    .bind(parse_id(id))
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    assert_eq!((attempts, dead), (1, false), "{error}");
    assert!(
        error.contains("handler panicked: the gate panics on"),
        "{error}"
    );

    sqlx::query("UPDATE jobs SET run_at = now() WHERE blob_id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    assert!(run_job(&app, agrafe::jobs::JobKind::Render).await);
    assert!(jobs_for(&app, id).await.is_empty(), "the job is done");
    let got = get_blob(&app, id).await;
    // The bytes are not a picture: a verdict all the same.
    assert_eq!(got["thumbnail_state"], "failed", "{got}");
}

/// Workers that render do not start on a renderer that fails its probe.
#[tokio::test]
async fn workers_refuse_a_broken_renderer() {
    let db = TestDatabase::new().await;
    let app = setup_tuned(db, fake_clamd().await, |c| {
        c.jobs.kinds = Some(vec![agrafe::jobs::JobKind::Render]);
        c.media.render.vips_command = "/nonexistent/vips".into();
    })
    .await;
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        agrafe::jobs::runner::run(app.state.clone(), std::future::pending::<()>()),
    )
    .await
    .expect("the workers give up at once")
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("the renderer failed its startup probe"),
        "{err}"
    );
}

/// Workers that pass their probes start, and the periodic jobs get
/// their first run: the reap at once, the audit one interval out.
#[tokio::test]
async fn workers_start_and_schedule_the_periodic_jobs() {
    use agrafe::jobs::JobKind;

    let db = TestDatabase::new().await;
    let app = setup_tuned(db, fake_clamd().await, |c| {
        c.jobs.kinds = Some(vec![
            JobKind::Render,
            JobKind::Document,
            JobKind::Reap,
            JobKind::Audit,
        ]);
        c.database.max_connections = 10;
    })
    .await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let shutdown = async move {
        let _ = stopped.await;
    }
    .boxed()
    .shared();
    let workers = tokio::spawn(agrafe::jobs::runner::run(app.state.clone(), shutdown));

    // The probes run first; the scheduling follows.
    let due_in = |kind: &'static str| {
        let pool = app.state.pool().clone();
        async move {
            sqlx::query_scalar::<_, f64>(
                "SELECT EXTRACT(EPOCH FROM run_at - now())::float8 FROM jobs \
                 WHERE kind = $1 AND failed_at IS NULL",
            )
            .bind(kind)
            .fetch_optional(&pool)
            .await
            .unwrap()
        }
    };
    let mut audit = None;
    for _ in 0..500 {
        audit = due_in("audit").await;
        if audit.is_some() {
            break;
        }
        assert!(!workers.is_finished(), "{:?}", workers.await);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let audit = audit.expect("the audit was scheduled");
    let interval = app.state.config().jobs.audit_interval.as_secs_f64();
    assert!(
        audit > interval - 60.0 && audit <= interval,
        "{audit} against {interval}"
    );
    // The reap ran at once, and scheduled the next one.
    let mut reap = None;
    for _ in 0..500 {
        reap = due_in("reap").await;
        if reap.is_some_and(|due| due > 0.0) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let reap = reap.expect("a reap is scheduled");
    let interval = app.state.config().jobs.reap_interval.as_secs_f64();
    assert!(reap > 0.0 && reap <= interval, "{reap} against {interval}");

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), workers)
        .await
        .expect("the workers stop")
        .unwrap()
        .unwrap();
}

/// The verdict on the blob's row follows the job that will finish the
/// blob, not the one that died: a job dying with a live twin says nothing,
/// a job succeeding takes a stale verdict with it, and a terminal
/// transition takes it too, since nothing is going to finish the blob.
#[tokio::test]
async fn the_blob_verdict_tracks_the_live_job() {
    let db = TestDatabase::new().await;
    use agrafe::jobs::{self, Failure, JobKind, Priority};

    let app = setup(db).await;
    let webhook = app.state.config().webhook.clone();
    let pool = app.state.pool();
    let now = jiff::Timestamp::now();
    let lease = Duration::from_secs(600);
    let created = create(&app, raw_blob()).await;
    let id = id_of(&created);
    let blob = parse_id(id);
    let mut conn = pool.acquire().await.unwrap();
    let enqueue = || {
        jobs::enqueue(
            pool,
            JobKind::Scan,
            Some(blob),
            Priority::Normal,
            json!({}),
            now,
        )
    };

    // A running job holds no pending slot, so a twin can be enqueued; the
    // first one dying for good is not the blob's verdict.
    assert!(enqueue().await.unwrap());
    let first = jobs::claim(pool, JobKind::Scan, "w1", lease)
        .await
        .unwrap()
        .unwrap();
    assert!(enqueue().await.unwrap());
    assert_eq!(
        jobs::fail(&mut conn, &first, "boom", true, None, &webhook, now)
            .await
            .unwrap(),
        Failure::Dead
    );
    let got = get_blob(&app, id).await;
    assert!(got["failed_at"].is_null(), "{got}");

    // The twin succeeding clears whatever verdict landed on the row.
    sqlx::query("UPDATE blobs SET failed_at = now(), last_error = 'stale' WHERE id = $1")
        .bind(blob)
        .execute(pool)
        .await
        .unwrap();
    let twin = jobs::claim(pool, JobKind::Scan, "w1", lease)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(twin.id, first.id);
    jobs::succeed(&mut conn, &twin).await.unwrap();
    let got = get_blob(&app, id).await;
    assert!(got["failed_at"].is_null(), "{got}");
    assert!(got["last_error"].is_null(), "{got}");

    // With no twin, the death is recorded, and a delete forgets it.
    sqlx::query("DELETE FROM jobs WHERE blob_id = $1")
        .bind(blob)
        .execute(pool)
        .await
        .unwrap();
    assert!(enqueue().await.unwrap());
    let alone = jobs::claim(pool, JobKind::Scan, "w1", lease)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        jobs::fail(&mut conn, &alone, "boom", true, None, &webhook, now)
            .await
            .unwrap(),
        Failure::Dead
    );
    let got = get_blob(&app, id).await;
    assert_eq!(got["last_error"], "boom", "{got}");
    assert_eq!(delete(&app, id).await, StatusCode::NO_CONTENT);
    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "deleted");
    assert!(got["failed_at"].is_null(), "{got}");
    assert!(got["last_error"].is_null(), "{got}");
    let (status, body) = call(
        &app.private,
        req(axum::http::Method::GET, "/blobs?failed=true"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 0, "{body}");
}

/// The body-less `POST`s of the private API are what a cross-site form
/// can reach without a preflight; the router refuses those a browser
/// marks as another site's, and only those.
#[tokio::test]
async fn a_cross_site_form_post_is_refused() {
    let db = TestDatabase::new().await;
    use axum::body::Body;
    use axum::http::{Method, Request, header};

    let app = setup(db).await;
    let created = create(&app, raw_blob()).await;
    let id = id_of(&created);
    let form = |method: Method, uri: &str, headers: &[(&str, &str)]| {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "agrafe.internal:8080")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        req.body(Body::empty()).unwrap()
    };

    for uri in [
        format!("/blobs/{id}/retry"),
        format!("/blobs/{id}/finish"),
        format!("/blobs/{id}/urls"),
        "/jobs/1/retry".to_string(),
    ] {
        for headers in [
            vec![("origin", "http://evil.test")],
            vec![("sec-fetch-site", "cross-site")],
            vec![
                ("sec-fetch-site", "same-site"),
                ("origin", "http://agrafe.internal:8080"),
            ],
        ] {
            let (status, body) = call(&app.private, form(Method::POST, &uri, &headers)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri} {headers:?}: {body}");
            assert_eq!(body["error"]["code"], "forbidden", "{body}");
        }
    }
    // A read is never refused, whoever asks.
    let (status, _) = call(
        &app.private,
        form(Method::GET, "/jobs", &[("origin", "http://evil.test")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The dashboard's own forms, and the backend with no browser headers,
    // reach the handler: this blob is `created`, so the retry is a `409`.
    for headers in [
        vec![],
        vec![("sec-fetch-site", "same-origin")],
        vec![("origin", "http://agrafe.internal:8080")],
    ] {
        let (status, body) = call(
            &app.private,
            form(Method::POST, &format!("/blobs/{id}/retry"), &headers),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{headers:?}: {body}");
    }
}

/// The claim reads the head of its kind's queue off `jobs_claimable_idx`,
/// in the index's order, even under the generic plan a pooled connection
/// may settle on: no sort, which under `FOR UPDATE` the `LIMIT` could not
/// bound, so a backlog of millions would be sorted at every claim.
/// Sequential and bitmap scans are off so that, on a table this small, the
/// planner shows the index it would walk.
#[tokio::test]
async fn the_claim_walks_its_index_under_a_generic_plan() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let mut conn = app.state.pool().acquire().await.unwrap();
    for off in ["SET enable_seqscan = off", "SET enable_bitmapscan = off"] {
        sqlx::query(off).execute(&mut *conn).await.unwrap();
    }
    let plans = agrafe::jobs::explain_queue(&mut conn).await.unwrap();
    let want = [
        ("claim", "jobs_claimable_idx"),
        ("exhausted", "jobs_exhausted_idx"),
        ("oldest_due", "jobs_claimable_idx"),
    ];
    assert_eq!(plans.len(), want.len());
    for ((name, plan), (want_name, index)) in plans.iter().zip(want) {
        assert_eq!(*name, want_name);
        assert!(plan.contains(index), "{name}: {plan}");
        assert!(!plan.contains("Seq Scan"), "{name}: {plan}");
        assert!(!plan.contains("Sort"), "{name}: {plan}");
    }
}

/// A job whose last attempt took its worker down, a process killed or out
/// of memory, never reaches `fail`: the claim does not take it again, and
/// the reaper declares it dead once its lease lapses, with the blob's
/// verdict, as if its handler had failed.
#[tokio::test]
async fn a_job_whose_attempts_ended_their_worker_dies() {
    use agrafe::jobs::{self, JobKind};
    use axum::http::Method;

    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let pool = app.state.pool();
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob);
    assert_eq!(jobs_for(&app, id).await, ["render"]);
    // Its last attempt claimed by a worker that died an hour ago.
    sqlx::query(
        "UPDATE jobs SET attempts = max_attempts, locked_by = 'gone', \
         locked_at = now() - interval '1 hour' WHERE blob_id = $1",
    )
    .bind(parse_id(id))
    .execute(pool)
    .await
    .unwrap();
    let lease = std::time::Duration::from_secs(600);
    assert!(
        jobs::claim(pool, JobKind::Render, "w1", lease)
            .await
            .unwrap()
            .is_none()
    );

    let report = jobs::reap::reap(&app.state, jiff::Timestamp::now())
        .await
        .unwrap();
    assert_eq!(report.exhausted, 1, "{report:?}");
    let (status, body) = call(
        &app.private,
        req(Method::GET, &format!("/jobs?blob_id={id}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["state"], "dead", "{body}");
    let got = get_blob(&app, id).await;
    assert_eq!(
        got["last_error"], "every attempt ended its worker before it finished",
        "{got}"
    );
}

/// The census counts each state as `Job::state` tells it, a lapsed lease
/// as unclaimed, and the worker-only process's ops router serves it too.
#[tokio::test]
async fn the_census_tells_every_state_and_the_ops_router_serves_it() {
    use axum::http::Method;

    let db = TestDatabase::new().await;
    let app = setup(db).await;
    // The test lease is ten minutes: half an hour ago is lapsed.
    sqlx::query(
        "INSERT INTO jobs (kind, run_at, locked_at, locked_by, failed_at) VALUES \
         ('reap', now() + interval '1 hour', NULL, NULL, NULL), \
         ('audit', now() - interval '1 hour', now(), 'w', now()), \
         ('purge', now() - interval '1 minute', now(), 'w', NULL), \
         ('notify', now() - interval '1 minute', now() - interval '30 minutes', 'w', NULL)",
    )
    .execute(app.state.pool())
    .await
    .unwrap();
    let ops = agrafe_axum::ops_router(app.state.clone());
    let (status, body) = call(&ops, req(Method::GET, "/healthz")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(&ops, req(Method::GET, "/metrics")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let text = body.as_str().unwrap();
    for (kind, state) in [
        ("reap", "scheduled"),
        ("audit", "dead"),
        ("purge", "running"),
        ("notify", "due"),
    ] {
        let line =
            format!("agrafe_jobs{{kind=\"{kind}\",priority=\"normal\",state=\"{state}\"}} 1");
        assert!(text.contains(&line), "{line}\n{text}");
    }
    let oldest = text
        .lines()
        .find(|l| {
            l.starts_with("agrafe_jobs_oldest_due_seconds{kind=\"notify\",priority=\"normal\"}")
        })
        .unwrap_or_else(|| panic!("{text}"));
    let age: f64 = oldest.rsplit(' ').next().unwrap().parse().unwrap();
    assert!(age >= 60.0, "{oldest}");
}
