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
    use agrafe::jobs::{self, Failure, JobKind};

    let app = setup().await;
    let pool = app.state.pool();
    let now = jiff::Timestamp::now();
    let lease = Duration::from_secs(600);
    let created = create(&app, raw_blob()).await;
    let blob = parse_id(id_of(&created));

    // One pending job per kind and blob; blob-less jobs are never deduped.
    assert!(
        jobs::enqueue(pool, JobKind::Scan, Some(blob), json!({}), now)
            .await
            .unwrap()
    );
    assert!(
        !jobs::enqueue(pool, JobKind::Scan, Some(blob), json!({}), now)
            .await
            .unwrap()
    );
    assert!(
        jobs::enqueue(pool, JobKind::Purge, None, json!({}), now)
            .await
            .unwrap()
    );
    assert!(
        jobs::enqueue(pool, JobKind::Purge, None, json!({}), now)
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
            JobKind::Thumbnail,
            Some(blob),
            json!({}),
            now + Duration::from_secs(3600)
        )
        .await
        .unwrap()
    );
    assert!(
        jobs::claim(pool, &[JobKind::Thumbnail], "w1", lease)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        jobs::claim(pool, &[JobKind::Preview], "w1", lease)
            .await
            .unwrap()
            .is_none()
    );

    // Failing unlocks with backoff; the next retry needs the lease to pass or
    // the backoff to elapse.
    let mut conn = pool.acquire().await.unwrap();
    let Failure::Retry(at) = jobs::fail(&mut conn, &again, "boom", now).await.unwrap() else {
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
        jobs::enqueue(pool, JobKind::Scan, Some(blob), json!({}), now)
            .await
            .unwrap()
    );
    assert_eq!(
        jobs::fail(&mut conn, &held, "boom", now).await.unwrap(),
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
    assert_eq!(purges, 1);
}
