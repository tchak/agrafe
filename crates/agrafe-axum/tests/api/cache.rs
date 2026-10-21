//! The rows the read routes keep: settled ones, for the configured TTL.

use std::time::Duration;

use agrafe::jobs::JobKind;
use axum::http::{Method, StatusCode};

use crate::support::*;

async fn hits(app: &TestApp) -> u64 {
    let (status, body) = call(&app.private, req(Method::GET, "/metrics")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_str()
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("agrafe_blob_head_cache_total{result=\"hit\"} "))
        .unwrap()
        .parse()
        .unwrap()
}

async fn status_of(app: &TestApp, uri: &str) -> StatusCode {
    call(&app.public, req(Method::GET, uri)).await.0
}

#[tokio::test]
async fn settled_rows_are_kept_for_the_ttl() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.blobs.read_cache_ttl = Duration::from_secs(1)
    })
    .await;
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let metadata = public_url(&blob, "metadata");

    // A render is pending: the row is in flux and not kept.
    assert_eq!(status_of(&app, &metadata).await, StatusCode::OK);
    assert_eq!(status_of(&app, &metadata).await, StatusCode::OK);
    assert_eq!(hits(&app).await, 0);

    // Settled: the second lookup is a hit, and a state recorded by another
    // process is seen once the TTL has passed.
    while run_job(&app, JobKind::Render).await {}
    assert_eq!(status_of(&app, &metadata).await, StatusCode::OK);
    assert_eq!(status_of(&app, &metadata).await, StatusCode::OK);
    assert_eq!(hits(&app).await, 1);
    sqlx::query("UPDATE blobs SET state = 'deleted' WHERE id = $1")
        .bind(parse_id(id_of(&blob)))
        .execute(app.state.pool())
        .await
        .unwrap();
    assert_eq!(status_of(&app, &metadata).await, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(status_of(&app, &metadata).await, StatusCode::NOT_FOUND);

    // A delete through this process is seen at once.
    let blob = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let metadata = public_url(&blob, "metadata");
    while run_job(&app, JobKind::Render).await {}
    assert_eq!(status_of(&app, &metadata).await, StatusCode::OK);
    assert_eq!(delete(&app, id_of(&blob)).await, StatusCode::NO_CONTENT);
    assert_eq!(status_of(&app, &metadata).await, StatusCode::NOT_FOUND);

    // An id that is not there is kept like the others. The private port
    // asks the same question, with no signature in the way.
    let hits_before = hits(&app).await;
    let missing = format!("/blobs/{}/thumbnail/x.jpg", uuid::Uuid::now_v7());
    for _ in 0..2 {
        let (status, _) = call(&app.private, req(Method::GET, &missing)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    assert_eq!(hits(&app).await, hits_before + 1);
}
