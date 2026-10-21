//! The trace context a request hands to the jobs it enqueues, and a job to
//! the jobs it enqueues in turn.

use agrafe::jobs::JobKind;
use agrafe::trace::TraceContext;
use axum::http::{Method, StatusCode};

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
#[sqlx::test(migrator = "MIGRATOR")]
async fn a_private_request_hands_its_callers_trace_to_its_jobs(pool: PgPool) {
    let app = setup(pool).await;
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
#[sqlx::test(migrator = "MIGRATOR")]
async fn a_request_without_a_valid_traceparent_starts_a_trace(pool: PgPool) {
    let app = setup(pool).await;
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
#[sqlx::test(migrator = "MIGRATOR")]
async fn the_public_interface_ignores_the_callers_trace(pool: PgPool) {
    let app = setup(pool).await;
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
