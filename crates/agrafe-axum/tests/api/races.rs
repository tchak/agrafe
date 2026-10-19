//! The row changing under an upload or a build: in the moment between the
//! last ownership check and the write that relies on it, or while the
//! bytes are still flowing. The object store holds the operation that
//! follows the check, and the test changes the row meanwhile.

use std::sync::Arc;
use std::time::Duration;

use agrafe::jobs::JobKind;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::support::*;

/// A `PUT` of `bytes` to `uri` on the private port, in a task of its own so
/// the test can act while it is held.
fn spawn_put(app: &TestApp, uri: String, bytes: Vec<u8>) -> JoinHandle<(StatusCode, Value)> {
    let router = app.private.clone();
    tokio::spawn(async move { call(&router, put_raw(&uri, bytes)).await })
}

/// One job of `kind`, in a task of its own.
fn spawn_job(app: &TestApp, kind: JobKind) -> JoinHandle<bool> {
    let state = app.state.clone();
    tokio::spawn(async move {
        agrafe::jobs::runner::run_one(&state, kind, "test")
            .await
            .unwrap()
    })
}

/// Run `query` with the blob's id as `$1`.
async fn sql(app: &TestApp, query: &'static str, id: &str) {
    sqlx::query(query)
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
}

/// Another upload claimed the row: still `uploading`, under another start.
const TAKE_OVER: &str =
    "UPDATE blobs SET upload_started_at = upload_started_at + interval '1 second' WHERE id = $1";
/// The row is gone altogether.
const REMOVE: &str = "DELETE FROM blobs WHERE id = $1";
/// Another build of the blob finished first.
const BUILT: &str = "UPDATE blobs SET state = 'ready' WHERE id = $1";

async fn stored(app: &TestApp, key: &str) -> bool {
    stored_objects(app)
        .await
        .iter()
        .any(|meta| meta.location.as_ref() == key)
}

/// Hold the completion of the blob's original: the task that would
/// complete it waits forever, and `finishes` fails on it. The object must
/// never be finished, not merely deleted afterwards.
fn never_completed(app: &TestApp, id: &str) {
    app.gate.hold(At::Complete, &format!("{id}/original"));
}

async fn finishes<T>(task: JoinHandle<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the object was never completed")
        .unwrap()
}

/// A `raw` blob that skips the scan, and its bytes.
async fn raw(app: &TestApp) -> (String, Vec<u8>) {
    let bytes = png_bytes(3000);
    let mut blob = blob_for(&bytes, "image/png", "raw");
    blob["skip_virus_scan"] = json!(true);
    let created = create(app, blob).await;
    (id_of(&created).to_string(), bytes)
}

/// Upload `bytes` to the blob, held as the object is about to become
/// visible, with `change` applied to the row meanwhile.
async fn upload_changed_at_completion(
    app: &TestApp,
    id: &str,
    bytes: Vec<u8>,
    change: impl AsyncFnOnce(),
) -> (StatusCode, Value) {
    let hold = app.gate.hold(At::Complete, &format!("{id}/original"));
    let put = spawn_put(app, format!("/blobs/{id}"), bytes);
    hold.reached().await;
    change().await;
    hold.release();
    put.await.unwrap()
}

#[tokio::test]
async fn upload_deleted_as_it_completes_leaves_nothing() {
    let app = setup().await;
    let (id, bytes) = raw(&app).await;
    let (status, body) = upload_changed_at_completion(&app, &id, bytes, async || {
        assert_eq!(delete(&app, &id).await, StatusCode::NO_CONTENT);
    })
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(get_blob(&app, &id).await["state"], "deleted");
    assert!(
        stored_objects(&app).await.is_empty(),
        "the object is disowned"
    );
}

#[tokio::test]
async fn upload_taken_over_as_it_completes_leaves_the_object_to_its_successor() {
    let app = setup().await;
    let (id, bytes) = raw(&app).await;
    let (status, body) = upload_changed_at_completion(&app, &id, bytes, async || {
        sql(&app, TAKE_OVER, &id).await;
    })
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(get_blob(&app, &id).await["state"], "uploading");
    assert!(
        stored(&app, &format!("{id}/original")).await,
        "the key is the successor's too"
    );
}

#[tokio::test]
async fn upload_whose_row_goes_as_it_completes_leaves_nothing() {
    let app = setup().await;
    let (id, bytes) = raw(&app).await;
    let (status, body) = upload_changed_at_completion(&app, &id, bytes, async || {
        sql(&app, REMOVE, &id).await;
    })
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(stored_objects(&app).await.is_empty());
}

/// The row changes while clamd holds the verdict: the check before the
/// object is finished catches it, and the multipart upload is aborted.
#[tokio::test]
async fn upload_that_loses_its_row_before_the_end_stores_nothing() {
    let received = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let clamd = fake_clamd_in(FakeClamd::Held {
        received: received.clone(),
        release: release.clone(),
    })
    .await;
    let app = setup_with(clamd).await;

    for (change, want) in [
        ("deleted", StatusCode::CONFLICT),
        ("removed", StatusCode::NOT_FOUND),
    ] {
        let bytes = png_bytes(1500);
        let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
        let id = id_of(&created).to_string();
        let put = spawn_put(&app, format!("/blobs/{id}"), bytes);
        within("clamd to receive the stream", received.notified()).await;
        match change {
            "deleted" => assert_eq!(delete(&app, &id).await, StatusCode::NO_CONTENT),
            _ => sql(&app, REMOVE, &id).await,
        }
        never_completed(&app, &id);
        release.notify_one();
        let (status, body) = finishes(put).await;
        assert_eq!(status, want, "{change}: {body}");
        assert!(stored_objects(&app).await.is_empty(), "{change}");
    }
}

/// The heartbeat notices the row taken away while the upload is stuck,
/// and stops it without waiting for the bytes.
#[tokio::test]
async fn heartbeat_stops_an_upload_that_lost_its_row() {
    let received = Arc::new(Notify::new());
    let clamd = fake_clamd_in(FakeClamd::Held {
        received: received.clone(),
        release: Arc::new(Notify::new()),
    })
    .await;
    let app = setup_tuned(clamd, |config| {
        config.upload.stale_after = Duration::from_secs(3);
    })
    .await;

    let bytes = png_bytes(1500);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let put = spawn_put(&app, format!("/blobs/{id}"), bytes);
    within("clamd to receive the stream", received.notified()).await;
    sql(&app, TAKE_OVER, id).await;
    // clamd is never released: only the heartbeat can end the upload.
    let (status, body) = tokio::time::timeout(Duration::from_secs(5), put)
        .await
        .expect("the heartbeat stops the upload")
        .unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(stored_objects(&app).await.is_empty());
}

/// A `rows` blob with one column.
async fn rows(app: &TestApp) -> String {
    let created = create(
        app,
        json!({
            "filename": "t.csv",
            "content_type": "text/csv",
            "input_type": "rows",
            "columns": [{"name": "A"}],
        }),
    )
    .await;
    id_of(&created).to_string()
}

const PART: &[u8] = b"[\"x\"]\n[\"y\"]\n";

#[tokio::test]
async fn part_whose_row_changes_as_it_completes_is_disowned() {
    let app = setup().await;
    for (change, want) in [
        ("deleted", StatusCode::CONFLICT),
        ("removed", StatusCode::NOT_FOUND),
    ] {
        let id = rows(&app).await;
        let hold = app.gate.hold(At::Complete, &format!("{id}/source/1"));
        let put = spawn_put(&app, format!("/blobs/{id}/parts/1"), PART.to_vec());
        hold.reached().await;
        match change {
            "deleted" => assert_eq!(delete(&app, &id).await, StatusCode::NO_CONTENT),
            _ => sql(&app, REMOVE, &id).await,
        }
        hold.release();
        let (status, body) = put.await.unwrap();
        assert_eq!(status, want, "{change}: {body}");
        assert!(stored_objects(&app).await.is_empty(), "{change}");
    }
}

/// A `rows` blob with its source in, waiting for its build.
async fn uploaded_rows(app: &TestApp) -> String {
    let id = rows(app).await;
    let (status, body) = call(
        &app.private,
        put_raw(&format!("/blobs/{id}/parts/1"), PART.to_vec()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &app.private,
        req(axum::http::Method::POST, &format!("/blobs/{id}/finish")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "uploaded");
    id
}

/// Deleted while the build reads the source: the build stops short of
/// finishing the object.
#[tokio::test]
async fn build_of_a_blob_deleted_mid_build_writes_nothing() {
    let app = setup().await;
    let id = uploaded_rows(&app).await;
    let hold = app.gate.hold(At::Get, &format!("{id}/source/1"));
    let job = spawn_job(&app, JobKind::Rows);
    hold.reached().await;
    assert_eq!(delete(&app, &id).await, StatusCode::NO_CONTENT);
    never_completed(&app, &id);
    hold.release();
    assert!(finishes(job).await);

    assert_eq!(get_blob(&app, &id).await["state"], "deleted");
    assert!(!stored(&app, &format!("{id}/original")).await);
}

/// The row changes as the built object is about to become visible: an
/// object nobody owns goes, one another build finished stays.
#[tokio::test]
async fn build_whose_row_changes_as_it_completes() {
    let app = setup().await;
    for (change, kept) in [("deleted", false), ("removed", false), ("built", true)] {
        let id = uploaded_rows(&app).await;
        let original = format!("{id}/original");
        let hold = app.gate.hold(At::Complete, &original);
        let job = spawn_job(&app, JobKind::Rows);
        hold.reached().await;
        match change {
            "deleted" => assert_eq!(delete(&app, &id).await, StatusCode::NO_CONTENT),
            "removed" => sql(&app, REMOVE, &id).await,
            _ => sql(&app, BUILT, &id).await,
        }
        hold.release();
        assert!(within("the job", job).await.unwrap(), "{change}");
        assert_eq!(stored(&app, &original).await, kept, "{change}");
    }
}
