//! The row changing under an upload or a build: in the moment between the
//! last ownership check and the write that relies on it, or while the
//! bytes are still flowing. The object store holds the operation that
//! follows the check, and the test changes the row meanwhile.

use std::sync::Arc;
use std::time::Duration;

use agrafe::jobs::JobKind;
use axum::http::{Method, StatusCode};
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
    app.gate.hold(At::Complete, &object_key(id, "original"));
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
    let hold = app.gate.hold(At::Complete, &object_key(id, "original"));
    let put = spawn_put(app, format!("/blobs/{id}"), bytes);
    hold.reached().await;
    change().await;
    hold.release();
    put.await.unwrap()
}

#[tokio::test]
async fn upload_deleted_as_it_completes_leaves_nothing() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
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
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let (id, bytes) = raw(&app).await;
    let (status, body) = upload_changed_at_completion(&app, &id, bytes, async || {
        sql(&app, TAKE_OVER, &id).await;
    })
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(get_blob(&app, &id).await["state"], "uploading");
    assert!(
        stored(&app, &object_key(&id, "original")).await,
        "the key is the successor's too"
    );
}

#[tokio::test]
async fn upload_whose_row_goes_as_it_completes_leaves_nothing() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
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
    let db = TestDatabase::new().await;
    let received = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let clamd = fake_clamd_in(FakeClamd::Held {
        received: received.clone(),
        release: release.clone(),
    })
    .await;
    let app = setup_with(db, clamd).await;

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
    let db = TestDatabase::new().await;
    let received = Arc::new(Notify::new());
    let clamd = fake_clamd_in(FakeClamd::Held {
        received: received.clone(),
        release: Arc::new(Notify::new()),
    })
    .await;
    let app = setup_tuned(db, clamd, |config| {
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
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    for (change, want) in [
        ("deleted", StatusCode::CONFLICT),
        ("removed", StatusCode::NOT_FOUND),
    ] {
        let id = rows(&app).await;
        let hold = app.gate.hold(At::Complete, &object_key(&id, "source/1"));
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
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let id = uploaded_rows(&app).await;
    let hold = app.gate.hold(At::Get, &object_key(&id, "source/1"));
    let job = spawn_job(&app, JobKind::Rows);
    hold.reached().await;
    assert_eq!(delete(&app, &id).await, StatusCode::NO_CONTENT);
    never_completed(&app, &id);
    hold.release();
    assert!(finishes(job).await);

    assert_eq!(get_blob(&app, &id).await["state"], "deleted");
    assert!(!stored(&app, &object_key(&id, "original")).await);
}

/// A render that outlives the purge of its blob: nothing visits the prefix
/// after the purge, so the render takes back what it writes there, and the
/// tombstone does not claim a representation.
#[tokio::test]
async fn render_that_outlives_the_purge_leaves_nothing() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("rotated.jpg")).await;
    let id = id_of(&blob).to_string();
    let hold = app.gate.hold(At::Put, &object_key(&id, "preview"));
    let job = spawn_job(&app, JobKind::Render);
    hold.reached().await;

    assert_eq!(delete(&app, &id).await, StatusCode::NO_CONTENT);
    sql(
        &app,
        "UPDATE jobs SET run_at = now() WHERE blob_id = $1 AND kind = 'purge'",
        &id,
    )
    .await;
    assert!(run_job(&app, JobKind::Purge).await);
    assert!(stored_objects(&app).await.is_empty());

    hold.release();
    assert!(job.await.unwrap());
    assert!(stored_objects(&app).await.is_empty());
    let states: Vec<String> =
        sqlx::query_scalar("SELECT state FROM representations WHERE blob_id = $1 ORDER BY kind")
            .bind(parse_id(&id))
            .fetch_all(app.state.pool())
            .await
            .unwrap();
    assert_eq!(states, ["pending", "pending"]);
}

/// Every due render, to the end.
async fn render_all(app: &TestApp) {
    while run_job(app, JobKind::Render).await {}
}

/// `GET` a representation through the private port, which is what asks for
/// a stale one.
async fn ask_for(app: &TestApp, id: &str, kind: &str) {
    let (status, _) = call(
        &app.private,
        req(Method::GET, &format!("/blobs/{id}/{kind}/photo.jpg")),
    )
    .await;
    assert!(
        status.is_success() || status == StatusCode::NOT_FOUND,
        "{status}"
    );
}

/// The render jobs of `id` as (locked, attempts, due in the future).
async fn renders(app: &TestApp, id: &str) -> Vec<(bool, i32, bool)> {
    sqlx::query_as(
        "SELECT locked_at IS NOT NULL, attempts, run_at > now() FROM jobs \
         WHERE blob_id = $1 AND kind = 'render' ORDER BY id",
    )
    .bind(parse_id(id))
    .fetch_all(app.state.pool())
    .await
    .unwrap()
}

/// Make the waiting render due now.
async fn due_now(app: &TestApp, id: &str) {
    sql(
        app,
        "UPDATE jobs SET run_at = now() WHERE blob_id = $1 AND kind = 'render'",
        id,
    )
    .await;
}

/// After a version bump the second representation's request enqueues a
/// twin while the first's render runs. It waits, its attempt refunded,
/// without decoding the original beside the other, and finds nothing left.
#[tokio::test]
async fn a_second_render_waits_for_the_one_running() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob).to_string();
    render_all(&app).await;
    sql(
        &app,
        "UPDATE representations SET version = 0 WHERE blob_id = $1",
        &id,
    )
    .await;

    ask_for(&app, &id, "thumbnail").await;
    let hold = app.gate.hold(At::Put, &object_key(&id, "preview"));
    let first = spawn_job(&app, JobKind::Render);
    hold.reached().await;
    ask_for(&app, &id, "preview").await;
    assert_eq!(
        renders(&app, &id).await,
        [(true, 1, false), (false, 0, false)]
    );

    // No gate holds this one, so it would finish if it rendered.
    assert!(run_job(&app, JobKind::Render).await);
    assert_eq!(
        renders(&app, &id).await,
        [(true, 1, false), (false, 0, true)],
        "unlocked, its attempt refunded, due later"
    );
    assert!(!run_job(&app, JobKind::Render).await, "not due yet");

    hold.release();
    assert!(within("the job", first).await.unwrap());
    let shown = get_blob(&app, &id).await;
    assert_eq!(
        shown["representations"]["thumbnail"]["state"], "ready",
        "{shown}"
    );
    assert_eq!(
        shown["representations"]["preview"]["state"], "ready",
        "{shown}"
    );

    due_now(&app, &id).await;
    assert!(run_job(&app, JobKind::Render).await);
    assert_eq!(renders(&app, &id).await, []);
}

/// What the running job read before the request is not its to render: the
/// twin, having waited, does it. Skipping the enqueue while a render runs
/// would leave the preview `pending` for good.
#[tokio::test]
async fn a_representation_asked_for_during_a_render_is_not_lost() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob).to_string();
    render_all(&app).await;
    sql(
        &app,
        "UPDATE representations SET version = 0 WHERE blob_id = $1 AND kind = 'thumbnail'",
        &id,
    )
    .await;

    // The thumbnail alone: its job uses the stored preview.
    ask_for(&app, &id, "thumbnail").await;
    let hold = app.gate.hold(At::Put, &object_key(&id, "thumbnail"));
    let first = spawn_job(&app, JobKind::Render);
    hold.reached().await;
    sql(
        &app,
        "UPDATE representations SET version = 0 WHERE blob_id = $1 AND kind = 'preview'",
        &id,
    )
    .await;
    ask_for(&app, &id, "preview").await;
    assert!(run_job(&app, JobKind::Render).await);

    hold.release();
    assert!(within("the job", first).await.unwrap());
    let shown = get_blob(&app, &id).await;
    assert_eq!(
        shown["representations"]["thumbnail"]["state"], "ready",
        "{shown}"
    );
    assert_eq!(
        shown["representations"]["preview"]["state"], "pending",
        "{shown}"
    );

    due_now(&app, &id).await;
    assert!(run_job(&app, JobKind::Render).await);
    assert_eq!(
        get_blob(&app, &id).await["representations"]["preview"]["state"],
        "ready"
    );
    assert_eq!(renders(&app, &id).await, []);
}

/// A worker that died holding the older render stops nothing once its
/// lease has lapsed.
#[tokio::test]
async fn a_lapsed_older_render_does_not_hold_the_next_one() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&blob).to_string();
    render_all(&app).await;
    // Out of attempts, so that the claim leaves it to the reaper.
    sql(
        &app,
        "INSERT INTO jobs (kind, blob_id, locked_at, locked_by, attempts, max_attempts) \
         VALUES ('render', $1, now() - interval '1 day', 'gone', 1, 1)",
        &id,
    )
    .await;
    sql(
        &app,
        "UPDATE representations SET version = 0 WHERE blob_id = $1 AND kind = 'preview'",
        &id,
    )
    .await;
    ask_for(&app, &id, "preview").await;
    assert!(run_job(&app, JobKind::Render).await);
    assert_eq!(
        get_blob(&app, &id).await["representations"]["preview"]["state"],
        "ready"
    );
}

/// The row changes as the built object is about to become visible: an
/// object nobody owns goes, one another build finished stays.
#[tokio::test]
async fn build_whose_row_changes_as_it_completes() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    for (change, kept) in [("deleted", false), ("removed", false), ("built", true)] {
        let id = uploaded_rows(&app).await;
        let original = object_key(&id, "original");
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
