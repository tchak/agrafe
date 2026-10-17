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
    let clamd = fake_clamd().await;
    let app = setup_tuned(clamd, |c| {
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

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), workers)
        .await
        .expect("the workers stop")
        .unwrap()
        .unwrap();
}

/// A job running longer than its lease keeps it: the heartbeat renews it,
/// so no other worker claims the job meanwhile.
#[tokio::test]
async fn running_job_keeps_its_lease() {
    let received = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let clamd = fake_clamd_in(FakeClamd::Held {
        received: received.clone(),
        release: release.clone(),
    })
    .await;
    // The heartbeat beats every second at the least.
    let lease = Duration::from_millis(1200);
    let app = setup_tuned(clamd, |c| c.jobs.lease_timeout = lease).await;

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
    received.notified().await;
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
    let stolen = agrafe::jobs::claim(
        app.state.pool(),
        &[agrafe::jobs::JobKind::Scan],
        "w2",
        lease,
    )
    .await
    .unwrap();
    assert!(stolen.is_none(), "another worker took the job: {stolen:?}");

    release.notify_one();
    assert!(job.await.unwrap());
    assert_eq!(get_blob(&app, id).await["state"], "ready");
}

#[tokio::test]
async fn job_queue_mechanics() {
    use agrafe::jobs::{self, Failure, JobKind, Priority};

    let app = setup().await;
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
    let job = jobs::claim(pool, &[JobKind::Scan], "w1", lease)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.kind, JobKind::Scan);
    assert_eq!(job.attempts, 1);
    assert_eq!(job.locked_by.as_deref(), Some("w1"));
    assert!(
        jobs::claim(pool, &[JobKind::Scan], "w2", lease)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE jobs SET locked_at = now() - interval '11 minutes' WHERE id = $1")
        .bind(job.id)
        .execute(pool)
        .await
        .unwrap();
    let again = jobs::claim(pool, &[JobKind::Scan], "w2", lease)
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
        jobs::claim(pool, &[JobKind::Scan], "w1", lease)
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
        jobs::claim(pool, &[JobKind::Render], "w1", lease)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        jobs::claim(pool, &[JobKind::Reap], "w1", lease)
            .await
            .unwrap()
            .is_none()
    );

    // Failing unlocks with backoff; the next retry needs the lease to pass or
    // the backoff to elapse.
    let mut conn = pool.acquire().await.unwrap();
    let Failure::Retry(at) = jobs::fail(&mut conn, &again, "boom", false, now)
        .await
        .unwrap()
    else {
        panic!("attempts remain");
    };
    assert!(at > now);
    assert!(
        jobs::claim(pool, &[JobKind::Scan], "w1", lease)
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
    let held = jobs::claim(pool, &[JobKind::Scan], "w1", lease)
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
    assert_eq!(
        jobs::fail(&mut conn, &held, "boom", false, now)
            .await
            .unwrap(),
        Failure::Superseded
    );
    let scans: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'scan'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(scans, 1);

    // Success removes the row.
    let purge = jobs::claim(pool, &[JobKind::Purge], "w1", lease)
        .await
        .unwrap()
        .unwrap();
    jobs::succeed(pool, purge.id).await.unwrap();
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
    use agrafe::jobs::{self, JobKind, Priority};
    use axum::http::Method;

    let app = setup().await;
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

    // Retrying makes the dead job due again, with a fresh attempt budget.
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
        &[JobKind::Scan],
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
    use axum::http::Method;

    let app = setup().await;
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

/// Within a kind, the highest priority goes first and age breaks ties; a
/// blob's priority travels onto its jobs.
#[tokio::test]
async fn priority_orders_a_kind_and_comes_from_the_blob() {
    use agrafe::jobs::{self, JobKind, Priority};
    use axum::http::Method;

    let app = setup().await;
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
    while let Some(job) = jobs::claim(pool, &[JobKind::Scan], "w", lease)
        .await
        .unwrap()
    {
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
