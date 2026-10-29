//! The pages against a real database, nested under `/dashboard` the way
//! `agrafe-server` mounts them. Each test runs on a database of its own,
//! created and migrated at its start on the server `DATABASE_URL` names
//! and dropped with the app.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use figment::Figment;
use figment::providers::{Format, Toml};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;
use uuid::Uuid;

use agrafe::Agrafe;
use agrafe::attachment::AttachmentRequest;
use agrafe::blob::InputType;
use agrafe::blob::create::CreateBlobRequest;
use agrafe::config::Config;
use agrafe::db::test::TestDatabase;
use agrafe::jobs::{self, JobKind, JobState, Priority};
use agrafe_dashboard::dashboard_router;

struct App {
    router: Router,
    state: Agrafe,
    storage: std::path::PathBuf,
    /// Dropped last, after the pool in `state`.
    _db: TestDatabase,
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.storage);
    }
}

/// The dashboard on `db`, the test's own database.
async fn setup(db: TestDatabase, actions: bool) -> App {
    let storage = std::env::temp_dir().join(format!("agrafe-dashboard-{}", Uuid::now_v7()));
    let key = STANDARD.encode([42u8; 32]);
    let toml = format!(
        r#"
        [server]
        private_addr = "127.0.0.1:0"
        public_addr = "127.0.0.1:0"
        public_base_url = "https://agrafe.test"
        [database]
        # Never dialled: the app runs on the pool of the test's database.
        url = "postgres://unused"
        [storage]
        backend = "local"
        [storage.local]
        path = "{tmp}"
        [signing]
        current_key = "k1"
        [signing.keys]
        k1 = "{key}"
        [encryption]
        current_kek = "kek1"
        [encryption.keks]
        kek1 = "{kek}"
        [dashboard]
        enabled = true
        actions = {actions}
        "#,
        kek = STANDARD.encode([43u8; 32]),
        tmp = storage.display(),
    );
    let config: Config = Figment::from(Toml::string(&toml)).extract().unwrap();
    let state = Agrafe::new(config, db.pool().clone()).unwrap();
    let router = Router::new().nest("/dashboard", dashboard_router(state.clone()));
    App {
        router,
        state,
        storage,
        _db: db,
    }
}

async fn send(app: &App, req: Request<Body>) -> (StatusCode, String, Option<String>) {
    let res = app.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let location = res
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap(), location)
}

async fn get(app: &App, uri: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let (status, body, _) = send(app, req).await;
    (status, body)
}

/// A page that rendered: `200`, HTML, the title in it.
async fn page(app: &App, uri: &str, title: &str) -> String {
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "{uri}");
    let content_type = res.headers()[header::CONTENT_TYPE].to_str().unwrap();
    assert!(
        content_type.starts_with("text/html"),
        "{uri}: {content_type}"
    );
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(body.contains(&format!("<title>{title}")), "{uri}:\n{body}");
    body
}

/// A state tab as the jobs page renders it: the label and its count.
fn tab(label: &str, count: i64) -> String {
    format!(r#"{label} <span class="badge badge-sm ml-1">{count}</span>"#)
}

/// A `POST` with the headers a browser sends from the dashboard's own
/// pages, plus `extra`.
fn post(uri: &str, extra: &[(&str, &str)]) -> Request<Body> {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::HOST, "agrafe.internal:8080")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    for (name, value) in extra {
        req = req.header(*name, *value);
    }
    req.body(Body::empty()).unwrap()
}

async fn create_blob(app: &App, filename: &str) -> Uuid {
    let req = CreateBlobRequest {
        filename: filename.to_string(),
        content_type: "image/png".to_string(),
        input_type: InputType::Raw,
        checksum: Some(STANDARD.encode([1u8; 32])),
        checksum_md5: None,
        size: Some(1024),
        url: None,
        columns: None,
        sheets: None,
        main: None,
        skip_virus_scan: false,
        expires_at: None,
        expires_in: None,
        priority: Priority::Normal,
        representations: None,
        attachment: None,
    };
    app.state.create_blob(req).await.unwrap().id
}

async fn attach(app: &App, blob: Uuid, owner_id: &str) -> Uuid {
    let req = AttachmentRequest {
        owner_kind: "document".to_string(),
        owner_id: owner_id.to_string(),
        role: "file".to_string(),
        replace: false,
    };
    app.state
        .attach_blob(blob, req)
        .await
        .unwrap()
        .attached
        .attachment()
        .id
}

/// A scan job for `blob` that ran out of attempts, as `jobs::fail` leaves
/// it, with the failure recorded on the blob too.
async fn dead_job(app: &App, blob: Uuid) -> i64 {
    let pool = app.state.pool();
    jobs::enqueue(
        pool,
        JobKind::Scan,
        Some(blob),
        Priority::Normal,
        json!({}),
        jiff::Timestamp::now(),
    )
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar(
        "UPDATE jobs SET failed_at = now(), locked_at = now(), locked_by = 'w', \
                         attempts = 5, last_error = E'boom\\nand the stack' \
         WHERE blob_id = $1 AND kind = 'scan' RETURNING id",
    )
    .bind(blob)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("UPDATE blobs SET failed_at = now(), last_error = 'boom' WHERE id = $1")
        .bind(blob)
        .execute(pool)
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn every_page_renders_with_and_without_filters() {
    let db = TestDatabase::new().await;
    let app = setup(db, false).await;
    let blob = create_blob(&app, "photo été.png").await;
    let other = create_blob(&app, "plain.png").await;
    let attachment = attach(&app, blob, "42").await;
    let dead = dead_job(&app, blob).await;
    jobs::enqueue(
        app.state.pool(),
        JobKind::Purge,
        None,
        Priority::Low,
        json!({}),
        jiff::Timestamp::now() + jiff::SignedDuration::from_secs(3600),
    )
    .await
    .unwrap();

    // The root goes to the queue; the stylesheet is served from the crate.
    let (status, _, location) = send(
        &app,
        Request::builder()
            .uri("/dashboard")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/dashboard/jobs"));
    let (status, css) = get(&app, "/dashboard/style.css").await;
    assert_eq!(status, StatusCode::OK);
    assert!(css.contains(".table"));

    // Jobs: the census, the tabs, the rows, the links to the blob.
    let body = page(&app, "/dashboard/jobs", "Jobs").await;
    assert!(body.contains(r#"href="/dashboard/style.css""#), "{body}");
    assert!(body.contains(&format!(r#"href="/dashboard/jobs/{dead}""#)));
    assert!(body.contains(&format!(r#"href="/dashboard/blobs/{blob}""#)));
    assert!(body.contains(">boom</td>"), "the first line only:\n{body}");
    assert!(
        body.contains(&tab("dead", 1)) && body.contains(&tab("scheduled", 1)),
        "{body}"
    );
    assert!(!body.contains("Retry"), "read-only:\n{body}");
    let body = page(
        &app,
        "/dashboard/jobs?state=dead&kind=scan&priority=normal",
        "Jobs",
    )
    .await;
    assert!(body.contains(&format!(r#"href="/dashboard/jobs/{dead}""#)));
    assert!(body.contains(r#"<option value="scan" selected>"#), "{body}");
    assert!(body.contains(r#"name="state" value="dead""#), "{body}");
    let body = page(&app, "/dashboard/jobs?state=due&refresh=5", "Jobs").await;
    assert!(body.contains("No jobs match."), "{body}");
    assert!(body.contains("Longest waiting first"), "{body}");
    assert!(
        body.contains(r#"<meta http-equiv="refresh" content="5">"#),
        "{body}"
    );
    assert!(
        body.contains(r#"href="/dashboard/jobs?refresh=5""#),
        "the tabs keep the filters:\n{body}"
    );
    // Five seconds at the least: each reload runs the census again.
    let body = page(&app, "/dashboard/jobs?refresh=1", "Jobs").await;
    assert!(
        body.contains(r#"<meta http-equiv="refresh" content="5">"#),
        "{body}"
    );
    let body = page(
        &app,
        &format!("/dashboard/jobs?blob_id={blob}&kind="),
        "Jobs",
    )
    .await;
    assert!(body.contains(&format!(r#"href="/dashboard/jobs/{dead}""#)));
    let body = page(
        &app,
        &format!("/dashboard/jobs/{dead}"),
        &format!("Job {dead}"),
    )
    .await;
    assert!(body.contains("and the stack"), "the whole error:\n{body}");
    assert!(body.contains(">{}</pre>"), "the payload:\n{body}");
    assert!(!body.contains("/discard"), "read-only:\n{body}");

    // Blobs: the filters, the attached flag, the detail with its
    // thumbnail from the API next door, its attachments and its jobs.
    let body = page(&app, "/dashboard/blobs", "Blobs").await;
    assert!(body.contains(&format!(r#"href="/dashboard/blobs/{blob}""#)));
    assert!(body.contains(&format!(r#"href="/dashboard/blobs/{other}""#)));
    assert!(body.contains("photo été.png"), "{body}");
    assert!(
        body.contains("<td>yes</td>") && body.contains("<td>no</td>"),
        "{body}"
    );
    let body = page(
        &app,
        "/dashboard/blobs?content_type=image/png&category=image&state=created&scan_state=pending&attached=true&failed=true",
        "Blobs",
    )
    .await;
    assert!(body.contains(&format!(r#"href="/dashboard/blobs/{blob}""#)));
    assert!(!body.contains(&format!(r#"href="/dashboard/blobs/{other}""#)));
    assert!(body.contains(r#"<option value="true" selected>"#), "{body}");
    let body = page(&app, "/dashboard/blobs?attached=false&failed=", "Blobs").await;
    assert!(body.contains(&format!(r#"href="/dashboard/blobs/{other}""#)));
    assert!(!body.contains(&format!(r#"href="/dashboard/blobs/{blob}""#)));
    let body = page(
        &app,
        &format!("/dashboard/blobs/{blob}"),
        "Blob photo été.png",
    )
    .await;
    assert!(
        body.contains(&format!(
            r#"src="/blobs/{blob}/thumbnail/photo%20%C3%A9t%C3%A9.png""#
        )),
        "{body}"
    );
    assert!(body.contains(&format!(r#"href="/dashboard/attachments/{attachment}""#)));
    assert!(body.contains(&format!(r#"href="/dashboard/jobs/{dead}""#)));
    assert!(body.contains(">boom</pre>"), "{body}");
    assert!(!body.contains("/retry"), "read-only:\n{body}");

    // Attachments: the four filters, the blob's state on the row.
    let body = page(&app, "/dashboard/attachments", "Attachments").await;
    assert!(body.contains(&format!(r#"href="/dashboard/attachments/{attachment}""#)));
    assert!(body.contains(">created</span>"), "{body}");
    let body = page(
        &app,
        &format!("/dashboard/attachments?owner_kind=document&owner_id=42&role=file&blob_id={blob}"),
        "Attachments",
    )
    .await;
    assert!(body.contains(&format!(r#"href="/dashboard/attachments/{attachment}""#)));
    assert!(body.contains(r#"name="owner_id" value="42""#), "{body}");
    assert!(
        body.contains(r#"<option value="document" selected>"#),
        "{body}"
    );
    assert!(body.contains(r#"<option value="file" selected>"#), "{body}");
    // A value the table no longer has is still offered, so the link holds.
    let body = page(
        &app,
        "/dashboard/attachments?owner_kind=gone",
        "Attachments",
    )
    .await;
    assert!(body.contains(r#"<option value="gone" selected>"#), "{body}");
    assert!(body.contains(r#"<option value="document">"#), "{body}");
    let body = page(
        &app,
        "/dashboard/attachments?owner_kind=other",
        "Attachments",
    )
    .await;
    assert!(body.contains("No attachments match."), "{body}");
    page(
        &app,
        &format!("/dashboard/attachments/{attachment}"),
        &format!("Attachment {attachment}"),
    )
    .await;

    // What is not there, or not well asked for, is a page too.
    for (uri, status) in [
        ("/dashboard/jobs/999999", StatusCode::NOT_FOUND),
        ("/dashboard/jobs/abc", StatusCode::NOT_FOUND),
        (
            "/dashboard/blobs/00000000-0000-0000-0000-000000000000",
            StatusCode::NOT_FOUND,
        ),
        ("/dashboard/attachments/nope", StatusCode::NOT_FOUND),
        ("/dashboard/nothing", StatusCode::NOT_FOUND),
        ("/dashboard/jobs?state=sleeping", StatusCode::BAD_REQUEST),
        (
            "/dashboard/blobs?content_type=nonsense",
            StatusCode::BAD_REQUEST,
        ),
        ("/dashboard/attachments?owner_id=1", StatusCode::BAD_REQUEST),
        ("/dashboard/attachments?role=file", StatusCode::BAD_REQUEST),
    ] {
        let (got, body) = get(&app, uri).await;
        assert_eq!(got, status, "{uri}:\n{body}");
        assert!(
            body.contains("<title>"),
            "{uri}: a page, not a bare status:\n{body}"
        );
        assert!(
            body.contains(status.canonical_reason().unwrap()),
            "{uri}:\n{body}"
        );
    }
}

#[tokio::test]
async fn the_census_matches_the_counts() {
    let db = TestDatabase::new().await;
    let app = setup(db, false).await;
    let pool = app.state.pool();
    let now = jiff::Timestamp::now();
    let blobs = [
        create_blob(&app, "a.png").await,
        create_blob(&app, "b.png").await,
        create_blob(&app, "c.png").await,
    ];
    for blob in &blobs[..2] {
        jobs::enqueue(
            pool,
            JobKind::Render,
            Some(*blob),
            Priority::High,
            json!({}),
            now,
        )
        .await
        .unwrap();
    }
    dead_job(&app, blobs[2]).await;
    jobs::enqueue(
        pool,
        JobKind::Purge,
        None,
        Priority::Normal,
        json!({}),
        now + jiff::SignedDuration::from_secs(600),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET run_at = now() - interval '90 seconds' WHERE kind = 'render'")
        .execute(pool)
        .await
        .unwrap();

    let counts = app.state.job_counts().await.unwrap();
    for c in &counts {
        assert_eq!(
            c.oldest_due_seconds.is_some(),
            c.state == JobState::Due,
            "{c:?}"
        );
    }
    let census = agrafe_dashboard::jobs::Census::from_counts(&counts);
    assert_eq!(census.total(JobState::Due).count, 2);
    assert_eq!(census.total(JobState::Scheduled).count, 1);
    assert_eq!(census.total(JobState::Running).count, 0);
    assert_eq!(census.total(JobState::Dead).count, 1);
    let rows: Vec<(&str, &str, i64, i64, i64, i64)> = census
        .rows
        .iter()
        .map(|r| {
            (
                r.kind.as_str(),
                r.priority.as_str(),
                r.due.count,
                r.scheduled.count,
                r.running.count,
                r.dead.count,
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("scan", "normal", 0, 0, 0, 1),
            ("render", "high", 2, 0, 0, 0),
            ("purge", "normal", 0, 1, 0, 0),
        ]
    );
    let oldest = census.rows[1].oldest_due.unwrap();
    assert!((85.0..120.0).contains(&oldest), "{oldest}");

    let body = page(&app, "/dashboard/jobs", "Jobs").await;
    assert!(body.contains(&tab("all", 4)), "{body}");
    assert!(body.contains(&tab("due", 2)), "{body}");
    assert!(body.contains(&tab("scheduled", 1)), "{body}");
    assert!(body.contains(&tab("dead", 1)), "{body}");
    assert!(
        body.contains("<td>1m 30s</td>") || body.contains("<td>1m 31s</td>"),
        "{body}"
    );
}

#[tokio::test]
async fn actions_are_off_by_default_and_answer_404() {
    let db = TestDatabase::new().await;
    let app = setup(db, false).await;
    let blob = create_blob(&app, "a.png").await;
    let dead = dead_job(&app, blob).await;
    for uri in [
        format!("/dashboard/jobs/{dead}/retry"),
        format!("/dashboard/jobs/{dead}/discard"),
        format!("/dashboard/blobs/{blob}/retry"),
    ] {
        let (status, body, _) = send(&app, post(&uri, &[("sec-fetch-site", "same-origin")])).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}:\n{body}");
    }
    let state = app.state.find_job(dead).await.unwrap().unwrap();
    assert_eq!(app.state.job_state(&state), JobState::Dead, "untouched");
}

#[tokio::test]
async fn a_cross_site_action_is_refused() {
    let db = TestDatabase::new().await;
    let app = setup(db.clone(), true).await;
    let blob = create_blob(&app, "a.png").await;
    let dead = dead_job(&app, blob).await;
    let uri = format!("/dashboard/jobs/{dead}/retry");
    for headers in [
        vec![("origin", "http://evil.test")],
        vec![("sec-fetch-site", "cross-site")],
        vec![
            ("sec-fetch-site", "same-site"),
            ("origin", "http://agrafe.internal:8080"),
        ],
        vec![("origin", "null")],
    ] {
        let (status, body, _) = send(&app, post(&uri, &headers)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{headers:?}:\n{body}");
        assert!(body.contains("did not come from the dashboard"), "{body}");
        let job = app.state.find_job(dead).await.unwrap().unwrap();
        assert_eq!(
            app.state.job_state(&job),
            JobState::Dead,
            "{headers:?}: untouched"
        );
    }
    // The check comes before the lookup: nothing is learnt about an id.
    let (status, _, _) = send(
        &app,
        post(
            "/dashboard/jobs/999999/retry",
            &[("origin", "http://evil.test")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // And before the actions flag: a read-only dashboard refuses the same.
    let app = setup(db, false).await;
    let (status, _, _) = send(
        &app,
        post(
            "/dashboard/jobs/999999/retry",
            &[("origin", "http://evil.test")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// A pasted link names a notice by code; an unknown code is ignored, and
/// nothing a link says reaches the page.
#[tokio::test]
async fn a_notice_is_a_code() {
    let db = TestDatabase::new().await;
    let app = setup(db, true).await;
    let (status, body) = get(&app, "/dashboard/jobs?notice=job_retried").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Job queued again."), "{body}");
    let (status, body) = get(&app, "/dashboard/jobs?notice=Click%20here").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("Click here"), "{body}");
    assert!(!body.contains("alert"), "no notice at all:\n{body}");
}

/// A request with no browser marks at all is a non-browser client on the
/// trusted network, and goes through: `curl` can retry a job.
#[tokio::test]
async fn an_unmarked_post_is_accepted() {
    let db = TestDatabase::new().await;
    let app = setup(db, true).await;
    let blob = create_blob(&app, "a.png").await;
    let dead = dead_job(&app, blob).await;
    let (status, _, _) = send(&app, post(&format!("/dashboard/jobs/{dead}/retry"), &[])).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let job = app.state.find_job(dead).await.unwrap().unwrap();
    assert_eq!(app.state.job_state(&job), JobState::Due);
}

#[tokio::test]
async fn a_retry_redirects_and_the_job_is_due_again() {
    let db = TestDatabase::new().await;
    let app = setup(db, true).await;
    let blob = create_blob(&app, "a.png").await;
    let dead = dead_job(&app, blob).await;

    // The buttons show on a dead job and a failed blob.
    let body = page(
        &app,
        &format!("/dashboard/jobs/{dead}"),
        &format!("Job {dead}"),
    )
    .await;
    assert!(
        body.contains(&format!(r#"action="/dashboard/jobs/{dead}/retry""#)),
        "{body}"
    );
    assert!(
        body.contains(&format!(r#"action="/dashboard/jobs/{dead}/discard""#)),
        "{body}"
    );
    let body = page(&app, &format!("/dashboard/blobs/{blob}"), "Blob a.png").await;
    assert!(
        body.contains(&format!(r#"action="/dashboard/blobs/{blob}/retry""#)),
        "{body}"
    );

    // A browser without Sec-Fetch-Site, with a matching Origin.
    let (status, _, location) = send(
        &app,
        post(
            &format!("/dashboard/jobs/{dead}/retry"),
            &[("origin", "http://agrafe.internal:8080")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = location.unwrap();
    assert!(
        location.starts_with(&format!("/dashboard/jobs/{dead}?notice=")),
        "{location}"
    );
    let job = app.state.find_job(dead).await.unwrap().unwrap();
    assert_eq!(app.state.job_state(&job), JobState::Due);
    assert_eq!(job.attempts, 0);
    let blob_row = app.state.find_blob(blob).await.unwrap().unwrap();
    assert!(
        blob_row.failed_at.is_none(),
        "the blob's failure is cleared"
    );
    let body = page(&app, &location, &format!("Job {dead}")).await;
    assert!(body.contains("Job queued again."), "{body}");
    assert!(
        !body.contains("/discard"),
        "no buttons on a live job:\n{body}"
    );

    // Again: the job is not dead any more, said on the page, nothing done.
    let (status, _, location) = send(
        &app,
        post(
            &format!("/dashboard/jobs/{dead}/retry"),
            &[("sec-fetch-site", "same-origin")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let body = page(&app, &location.unwrap(), &format!("Job {dead}")).await;
    assert!(body.contains("Nothing done: only a dead job"), "{body}");

    // The blob's side: a created blob waits on no job, and the retry says
    // so rather than queueing anything.
    let (status, _, location) = send(
        &app,
        post(
            &format!("/dashboard/blobs/{blob}/retry"),
            &[("sec-fetch-site", "none")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let body = page(&app, &location.unwrap(), "Blob a.png").await;
    assert!(
        body.contains("Nothing queued: the blob is not waiting on a job."),
        "{body}"
    );

    // Discard: dead again, then gone, the listing told.
    sqlx::query("UPDATE jobs SET failed_at = now() WHERE id = $1")
        .bind(dead)
        .execute(app.state.pool())
        .await
        .unwrap();
    let (status, _, location) = send(
        &app,
        post(
            &format!("/dashboard/jobs/{dead}/discard"),
            &[("sec-fetch-site", "same-origin")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = location.unwrap();
    assert_eq!(location, "/dashboard/jobs?state=dead&notice=job_discarded");
    let body = page(&app, &location, "Jobs").await;
    assert!(body.contains("Job discarded."), "{body}");
    assert!(app.state.find_job(dead).await.unwrap().is_none());
    let (status, _, _) = send(
        &app,
        post(
            &format!("/dashboard/jobs/{dead}/discard"),
            &[("sec-fetch-site", "same-origin")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The blob's retry, now that its dead job is gone, queues the scan.
    sqlx::query("UPDATE blobs SET failed_at = now(), last_error = 'boom' WHERE id = $1")
        .bind(blob)
        .execute(app.state.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE blobs SET state = 'uploaded', storage_key = 'k' WHERE id = $1")
        .bind(blob)
        .execute(app.state.pool())
        .await
        .unwrap();
    let (status, _, location) = send(
        &app,
        post(
            &format!("/dashboard/blobs/{blob}/retry"),
            &[("sec-fetch-site", "same-origin")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let body = page(&app, &location.unwrap(), "Blob a.png").await;
    assert!(body.contains("Retry queued"), "{body}");
    let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM jobs WHERE blob_id = $1")
        .bind(blob)
        .fetch_all(app.state.pool())
        .await
        .unwrap();
    assert_eq!(kinds, ["scan"]);
}

/// Every field value of every span opened while it is the default
/// subscriber, as `Debug` prints it, whatever the span's level.
#[derive(Clone, Default)]
struct Fields(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .lock()
            .unwrap()
            .push(format!("{}={value:?}", field.name()));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Fields {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _: &tracing::span::Id,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        attrs.record(&mut self.clone());
    }
}

/// A page is one `request` span, the private interface's: its path, no
/// query string, and a trace id; no second span with the full URI.
#[tokio::test]
async fn a_page_runs_in_one_request_span_without_its_query() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let db = TestDatabase::new().await;
    let app = setup(db, false).await;
    let fields = Fields::default();
    let _default =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(fields.clone()));
    page(&app, "/dashboard/jobs?kind=scan&state=due", "Jobs").await;
    let fields = fields.0.lock().unwrap().clone();
    let paths: Vec<&String> = fields.iter().filter(|f| f.starts_with("path=")).collect();
    assert_eq!(paths, ["path=/dashboard/jobs"], "{fields:?}");
    assert!(
        fields.iter().all(|f| !f.contains("kind=scan")),
        "{fields:?}"
    );
    assert!(fields.iter().all(|f| !f.starts_with("uri=")), "{fields:?}");
}
