//! The trace context a request hands to the jobs it enqueues, and a job to
//! the jobs it enqueues in turn.

use agrafe::jobs::notify::{self, Event};
use agrafe::jobs::{JobKind, Priority, audit, reap};
use agrafe::trace::TraceContext;
use axum::http::{Method, StatusCode};
use jiff::Timestamp;

use crate::support::*;

const CALLER: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const CALLER_TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

/// The stored context of the one job of `kind` for the blob.
async fn context_of(app: &TestApp, id: &str, kind: JobKind) -> (i64, TraceContext) {
    let (job, traceparent): (i64, Option<String>) =
        sqlx::query_as("SELECT id, traceparent FROM jobs WHERE blob_id = $1 AND kind = $2")
            .bind(parse_id(id))
            .bind(kind.as_str())
            .fetch_one(app.state.pool())
            .await
            .unwrap();
    let traceparent = traceparent.expect("every job enqueued in a request has a context");
    (job, TraceContext::parse(&traceparent).unwrap())
}

/// A private request continues its caller's trace; the upload pipeline,
/// in a task of its own, enqueues the scan under it; the scan's run
/// enqueues the render under its own id, in the same trace.
#[tokio::test]
async fn a_private_request_hands_its_callers_trace_to_its_jobs() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    // Over the inline scan limit, so the scan is a job.
    let bytes = png_bytes(10 * 1024);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);

    let mut put = put_raw(&format!("/blobs/{id}"), bytes);
    put.headers_mut()
        .insert("traceparent", CALLER.parse().unwrap());
    let (status, body) = call(&app.private, put).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (scan_job, scan) = context_of(&app, id, JobKind::Scan).await;
    assert_eq!(scan.trace_id().to_string(), CALLER_TRACE);
    let caller = TraceContext::parse(CALLER).unwrap();
    assert_ne!(
        scan.span_id().to_string(),
        caller.span_id().to_string(),
        "the job's parent is the request, not the request's caller"
    );

    let (status, job) = call(&app.private, req(Method::GET, &format!("/jobs/{scan_job}"))).await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["trace_id"], CALLER_TRACE);

    assert!(run_scan_job(&app).await);
    let (_, render) = context_of(&app, id, JobKind::Render).await;
    assert_eq!(render.trace_id().to_string(), CALLER_TRACE);
    assert_ne!(
        render.span_id().to_string(),
        scan.span_id().to_string(),
        "the render's parent is the scan's run, not the request"
    );
}

/// Without a `traceparent`, or with one W3C says to ignore, the request
/// starts a trace, and its jobs still share it.
#[tokio::test]
async fn a_request_without_a_valid_traceparent_starts_a_trace() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let mut traces = Vec::new();
    for header in [
        None,
        Some("00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01"),
    ] {
        let bytes = png_bytes(10 * 1024);
        let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
        let id = id_of(&created);
        let mut put = put_raw(&format!("/blobs/{id}"), bytes);
        if let Some(header) = header {
            put.headers_mut()
                .insert("traceparent", header.parse().unwrap());
        }
        let (status, body) = call(&app.private, put).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (_, scan) = context_of(&app, id, JobKind::Scan).await;
        // Uppercase hex is malformed: not the same trace in another case.
        assert_ne!(scan.trace_id().to_string(), CALLER_TRACE, "{header:?}");
        assert!(scan.sampled());
        assert!(run_scan_job(&app).await);
        let (_, render) = context_of(&app, id, JobKind::Render).await;
        assert_eq!(render.trace_id().to_string(), scan.trace_id().to_string());
        traces.push(scan.trace_id().to_string());
    }
    assert_ne!(traces[0], traces[1], "each request a trace of its own");
}

/// The public interface's clients are browsers and anyone: a trace id is
/// not theirs to pick, so the request starts its own.
#[tokio::test]
async fn the_public_interface_ignores_the_callers_trace() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(10 * 1024);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);

    let mut put = put_raw(&public_url(&created, "upload"), bytes);
    put.headers_mut()
        .insert("traceparent", CALLER.parse().unwrap());
    let (status, body) = call(&app.public, put).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, scan) = context_of(&app, id, JobKind::Scan).await;
    assert_ne!(scan.trace_id().to_string(), CALLER_TRACE);
}

/// A periodic job schedules its successor from inside its own run. The
/// successor records no context, whatever is current, so each run starts a
/// trace instead of every run since the first chaining into one.
#[tokio::test]
async fn a_periodic_job_schedules_its_successor_outside_its_trace() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let pool = app.state.pool();
    async fn traceparents(pool: &PgPool) -> Vec<Option<String>> {
        sqlx::query_scalar("SELECT traceparent FROM jobs WHERE kind = 'reap'")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    // Scheduled from within a trace, as a run does.
    let scheduled = TraceContext::root()
        .scope(reap::schedule(pool, Timestamp::now()))
        .await;
    assert!(scheduled.unwrap());
    assert_eq!(traceparents(pool).await, [None]);

    // The run, in a trace of its own, leaves its successor without one.
    assert!(run_job(&app, JobKind::Reap).await);
    assert_eq!(traceparents(pool).await, [None], "the successor, rootless");
}

/// Every field value of every span opened while it is the default
/// subscriber, as `Debug` prints it.
#[derive(Clone, Default)]
struct Fields(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl Fields {
    fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

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

    fn on_record(
        &self,
        _: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        values.record(&mut self.clone());
    }
}

/// No public request span records a token, whatever the path: a URL that
/// is served, one refused, one cut short at the token, with or without its
/// slash, and the router nested under a prefix.
#[tokio::test]
async fn public_request_spans_never_record_the_token() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_blob(&app, &png_bytes(1500)).await;
    let original = public_url(&blob, "original");
    let token = original
        .strip_prefix("/blobs/")
        .and_then(|rest| rest.split('/').next())
        .unwrap()
        .to_string();
    let nested = axum::Router::new().nest("/files", agrafe_axum::public_router(app.state.clone()));

    let fields = Fields::default();
    let _default =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(fields.clone()));
    let (status, _) = call(&app.public, req(Method::GET, &original)).await;
    assert_eq!(status, StatusCode::OK);
    let tampered = original.replace("/original/", "/thumbnail/");
    let (status, _) = call(&app.public, req(Method::GET, &tampered)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for cut in [format!("/blobs/{token}"), format!("/blobs/{token}/")] {
        let (status, _) = call(&app.public, req(Method::GET, &cut)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{cut}");
    }
    let (status, _) = call(&nested, req(Method::GET, &format!("/files{original}"))).await;
    assert_eq!(status, StatusCode::OK);

    let fields = fields.all();
    // The request spans' paths; the store's spans record object keys.
    let paths: Vec<&String> = fields.iter().filter(|f| f.starts_with("path=/")).collect();
    assert_eq!(paths.len(), 5, "{fields:?}");
    assert!(
        paths.iter().all(|p| p.contains("/blobs/<token>")),
        "{paths:?}"
    );
    // The nested router's with its mount prefix.
    assert_eq!(
        paths
            .iter()
            .filter(|p| p.starts_with("path=/files/blobs/<token>/"))
            .count(),
        1,
        "{paths:?}"
    );
    assert!(fields.iter().all(|f| !f.contains(&token)), "{fields:?}");
}

const OTHER: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

/// A retry leaves the stored context alone: the job runs again in the
/// trace that enqueued it, not the retrying request's.
#[tokio::test]
async fn a_retry_keeps_the_jobs_trace() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let bytes = png_bytes(10 * 1024);
    let created = create(&app, blob_for(&bytes, "image/png", "raw")).await;
    let id = id_of(&created);
    let mut put = put_raw(&format!("/blobs/{id}"), bytes);
    put.headers_mut()
        .insert("traceparent", CALLER.parse().unwrap());
    let (status, body) = call(&app.private, put).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (job, _) = context_of(&app, id, JobKind::Scan).await;
    sqlx::query(
        "UPDATE jobs SET attempts = max_attempts, locked_at = now(), locked_by = 'w', \
         failed_at = now() WHERE id = $1",
    )
    .bind(job)
    .execute(app.state.pool())
    .await
    .unwrap();

    let mut retry = req(Method::POST, &format!("/jobs/{job}/retry"));
    retry
        .headers_mut()
        .insert("traceparent", OTHER.parse().unwrap());
    let (status, body) = call(&app.private, retry).await;
    assert!(status.is_success(), "{status}: {body}");
    let (_, context) = context_of(&app, id, JobKind::Scan).await;
    assert_eq!(context.trace_id().to_string(), CALLER_TRACE);
}

/// A notify that replaces the pending one takes the newer event's trace:
/// the delivery is that event's.
#[tokio::test]
async fn a_replacing_notify_takes_the_newer_events_trace() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, blob_for(b"x", "text/plain", "raw")).await;
    let id = id_of(&created);
    let pool = app.state.pool();
    let blob = parse_id(id);
    for (traceparent, event) in [(CALLER, Event::Ready), (OTHER, Event::Failed)] {
        TraceContext::parse(traceparent)
            .unwrap()
            .scope(notify::enqueue(
                pool,
                blob,
                Priority::Normal,
                event,
                Timestamp::now(),
            ))
            .await
            .unwrap();
    }
    let (_, context) = context_of(&app, id, JobKind::Notify).await;
    assert_eq!(
        context.trace_id().to_string(),
        TraceContext::parse(OTHER).unwrap().trace_id().to_string()
    );
}

/// The audit schedules its successor outside its trace, as the reap does.
#[tokio::test]
async fn the_audit_schedules_its_successor_outside_its_trace() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let pool = app.state.pool();
    let scheduled = TraceContext::root()
        .scope(audit::schedule(pool, Timestamp::now()))
        .await;
    assert!(scheduled.unwrap());
    let traceparents: Vec<Option<String>> =
        sqlx::query_scalar("SELECT traceparent FROM jobs WHERE kind = 'audit'")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(traceparents, [None]);
}

/// The private request span records the path without its query string
/// and the trace id of the caller it continues.
#[tokio::test]
async fn private_request_spans_record_the_path_and_the_trace() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let fields = Fields::default();
    let _default =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(fields.clone()));
    let mut list = req(Method::GET, "/blobs?limit=5");
    list.headers_mut()
        .insert("traceparent", CALLER.parse().unwrap());
    let (status, body) = call(&app.private, list).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let fields = fields.all();
    assert!(fields.iter().any(|f| f == "path=/blobs"), "{fields:?}");
    assert!(fields.iter().all(|f| !f.contains("limit=")), "{fields:?}");
    assert!(
        fields
            .iter()
            .any(|f| f == &format!("trace_id={CALLER_TRACE}")),
        "{fields:?}"
    );
}
