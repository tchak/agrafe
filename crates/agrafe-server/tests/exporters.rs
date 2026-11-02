//! The binary against a fake collector and a fake Sentry: everything the
//! exporters send, read as it crosses the wire, for what a public request
//! could leak and for the trace a request and its job make.
#![cfg(all(feature = "sentry", feature = "otel"))]

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agrafe::db::test::TestDatabase;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderName, StatusCode};
use axum::routing::post;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::trace::v1::Span;
use prost::Message as _;
use serde_json::{Value, json};
use tokio::net::TcpListener;

const FILENAME: &str = "secret-report.txt";
const ENVELOPE: &str = "d3JhcHBlZC1kYXRhLWtleQ";

type Bodies<T> = Arc<Mutex<Vec<T>>>;

/// What reached the fake endpoints, by kind.
#[derive(Clone, Default)]
struct Received {
    otlp_http: Bodies<(String, Vec<u8>)>,
    otlp_grpc: Bodies<Vec<u8>>,
    sentry: Bodies<Vec<u8>>,
}

impl Received {
    /// The spans of every export, whichever protocol carried them.
    fn spans(&self) -> Vec<Span> {
        let http = self.otlp_http.lock().unwrap();
        let grpc = self.otlp_grpc.lock().unwrap();
        http.iter()
            .map(|(_, body)| body.as_slice())
            // A gRPC message: a compression flag and a length, then protobuf.
            .chain(grpc.iter().map(|body| &body[5..]))
            .flat_map(|body| {
                ExportTraceServiceRequest::decode(body)
                    .expect("an OTLP export")
                    .resource_spans
            })
            .flat_map(|resource| resource.scope_spans)
            .flat_map(|scope| scope.spans)
            .collect()
    }

    /// Every byte received, spans and Sentry envelopes alike.
    fn everything(&self) -> Vec<u8> {
        let mut all = Vec::new();
        for (_, body) in self.otlp_http.lock().unwrap().iter() {
            all.extend_from_slice(body);
        }
        for body in self.otlp_grpc.lock().unwrap().iter() {
            all.extend_from_slice(body);
        }
        for body in self.sentry.lock().unwrap().iter() {
            all.extend_from_slice(body);
        }
        all
    }

    fn sentry_text(&self) -> String {
        let envelopes = self.sentry.lock().unwrap();
        String::from_utf8_lossy(&envelopes.concat()).into_owned()
    }
}

/// OTLP over HTTP on `/v1/traces` and gRPC on the service's method, and
/// Sentry's envelope endpoint, all on one address.
async fn fake_endpoints() -> (SocketAddr, Received) {
    let received = Received::default();
    let app = Router::new()
        .route(
            "/v1/traces",
            post(|State(received): State<Received>, body: Bytes| async move {
                let entry = ("/v1/traces".to_string(), body.to_vec());
                received.otlp_http.lock().unwrap().push(entry);
                StatusCode::OK
            }),
        )
        .route(
            "/opentelemetry.proto.collector.trace.v1.TraceService/Export",
            post(|State(received): State<Received>, body: Bytes| async move {
                received.otlp_grpc.lock().unwrap().push(body.to_vec());
                // A trailers-only answer: the headers carry the status.
                [
                    (HeaderName::from_static("content-type"), "application/grpc"),
                    (HeaderName::from_static("grpc-status"), "0"),
                ]
            }),
        )
        .route(
            "/api/{project}/envelope/",
            post(
                |UrlPath(_): UrlPath<String>, State(received): State<Received>, body: Bytes| async move {
                    received.sentry.lock().unwrap().push(body.to_vec());
                    StatusCode::OK
                },
            ),
        )
        .with_state(received.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, received)
}

async fn free_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

/// The server under test, stopped the way an orchestrator does on drop if
/// the test has not already.
struct Server {
    child: Child,
    private: SocketAddr,
    public: SocketAddr,
    log: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl Server {
    async fn start(
        db: &TestDatabase,
        exporters: impl FnOnce(SocketAddr) -> String,
        collector: SocketAddr,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("objects");
        std::fs::create_dir(&storage).unwrap();
        let (private, public) = (free_port().await, free_port().await);
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let name = db.connect_options().get_database().unwrap().to_string();
        let sep = if url.contains('?') { '&' } else { '?' };
        // clamd cannot be reached, so the upload's scan is a job, and one
        // attempt in this database, so its failure is the last: an `error`
        // event.
        sqlx::query("ALTER TABLE jobs ALTER COLUMN max_attempts SET DEFAULT 1")
            .execute(db.pool())
            .await
            .unwrap();
        let toml = format!(
            r#"
            [server]
            private_addr = "{private}"
            public_addr = "{public}"
            public_base_url = "http://{public}"
            [database]
            url = "{url}{sep}dbname={name}"
            max_connections = 4
            [storage]
            backend = "local"
            [storage.local]
            path = "{storage}"
            [clamav]
            addr = "127.0.0.1:1"
            [signing]
            current_key = "k1"
            [signing.keys]
            k1 = "KioqKioqKioqKioqKioqKioqKioqKioqKioqKioqKio="
            [encryption]
            current_kek = "kek1"
            [encryption.keks]
            kek1 = "KysrKysrKysrKysrKysrKysrKysrKysrKysrKysrKys="
            [jobs]
            poll_interval = "100ms"
            kinds = ["scan"]
            {exporters}
            "#,
            storage = storage.display(),
            exporters = exporters(collector),
        );
        let config = dir.path().join("agrafe.toml");
        std::fs::write(&config, toml).unwrap();
        let log = dir.path().join("server.log");
        let mut command = Command::new(env!("CARGO_BIN_EXE_agrafe"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("AGRAFE_") {
                command.env_remove(name);
            }
        }
        let child = command
            .arg("--config")
            .arg(&config)
            .arg("serve")
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            private,
            public,
            log,
            _dir: dir,
        };
        server.wait_healthy().await;
        server
    }

    async fn wait_healthy(&mut self) {
        let client = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let url = format!("http://{}/healthz", self.private);
            if client
                .get(url)
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return;
            }
            let exited = self.child.try_wait().unwrap();
            assert!(exited.is_none(), "exited: {}", self.logged());
            assert!(Instant::now() < deadline, "not healthy: {}", self.logged());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn logged(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// SIGTERM, and wait: what the exporters held must be sent by the exit.
    async fn stop(&mut self) {
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        assert!(status.success());
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "{status}: {}", self.logged());
                return;
            }
            assert!(
                Instant::now() < deadline,
                "still running: {}",
                self.logged()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The path and query of a URL the API minted.
fn target(url: &str) -> &str {
    let rest = url.split_once("://").expect("an absolute URL").1;
    &rest[rest.find('/').unwrap()..]
}

/// The token of a public path, `/blobs/<token>/…`.
fn token_of(path: &str) -> String {
    path.strip_prefix("/blobs/")
        .and_then(|rest| rest.split('/').next())
        .unwrap()
        .to_string()
}

fn attribute(span: &Span, key: &str) -> Option<String> {
    use opentelemetry_proto::tonic::common::v1::any_value::Value as Any;
    span.attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref()?.value.as_ref())
        .and_then(|value| match value {
            Any::StringValue(text) => Some(text.clone()),
            _ => None,
        })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A blob uploaded through its public URL, the scan left to a job that
/// dies, then read back through its public URL with an `e` on top. Returns
/// the public tokens and the `traceparent` the job row stored.
async fn exercise(server: &Server, db: &TestDatabase) -> ([String; 2], String) {
    let client = reqwest::Client::new();
    let created: Value = client
        .post(format!("http://{}/blobs", server.private))
        .json(&json!({
            "filename": FILENAME,
            "content_type": "text/plain",
            "input_type": "raw",
            // Of "hello".
            "checksum": "LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=",
            "size": 5,
        }))
        .send()
        .await
        .unwrap_or_else(|err| panic!("{err}: {}", server.logged()))
        .json()
        .await
        .unwrap();
    let upload = created["urls"]["upload"]
        .as_str()
        .unwrap_or_else(|| panic!("no upload URL in {created}"));
    let upload = target(upload).to_string();
    let token = token_of(&upload);

    let response = client
        .put(format!("http://{}{upload}", server.public))
        .header("content-type", "text/plain")
        .body("hello")
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{:?}",
        response.text().await
    );
    // The body names the read URL: its token and its filename go through
    // the same span.
    let original = created["urls"]["original"]
        .as_str()
        .map(target)
        .unwrap()
        .to_string();
    assert!(original.contains(FILENAME), "{original}");
    let read = client
        .get(format!("http://{}{original}?e={ENVELOPE}", server.public))
        .send()
        .await
        .unwrap();
    assert!(!read.status().is_server_error(), "{}", read.status());

    // The scan's one attempt has run and died.
    let deadline = Instant::now() + Duration::from_secs(30);
    let traceparent = loop {
        let row: Option<(Option<String>,)> = sqlx_query(db).await;
        if let Some((Some(traceparent),)) = row {
            break traceparent;
        }
        assert!(
            Instant::now() < deadline,
            "no dead scan: {}",
            server.logged()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    ([token, token_of(&original)], traceparent)
}

async fn sqlx_query(db: &TestDatabase) -> Option<(Option<String>,)> {
    sqlx::query_as("SELECT traceparent FROM jobs WHERE kind = 'scan' AND failed_at IS NOT NULL")
        .fetch_optional(db.pool())
        .await
        .unwrap()
}

fn assert_clean(received: &Received, tokens: &[String; 2]) {
    let everything = received.everything();
    assert!(!everything.is_empty());
    let text = String::from_utf8_lossy(&everything);
    // The last is the dead job's field, which may quote a file.
    for secret in [
        &tokens[0],
        &tokens[1],
        ENVELOPE,
        FILENAME,
        "secret-report",
        "last_error",
    ] {
        assert!(!text.contains(secret), "{secret} left the process: {text}");
    }
}

/// The request's span, the job's, and the row between them: one trace, the
/// row naming the request's span, and the job's span its child.
fn assert_one_trace(received: &Received, traceparent: &str) {
    let spans = received.spans();
    let upload = spans
        .iter()
        .find(|span| {
            attribute(span, "http.route").is_some_and(|route| route.contains("{token}"))
                && attribute(span, "method").as_deref() == Some("PUT")
        })
        .unwrap_or_else(|| panic!("no upload span in {spans:#?}"));
    assert!(upload.name.starts_with("PUT /blobs/{token}"), "{upload:?}");
    assert_eq!(attribute(upload, "path"), None, "{upload:?}");

    let job = spans
        .iter()
        .find(|span| span.name == "job scan")
        .unwrap_or_else(|| panic!("no job span in {spans:#?}"));
    assert_eq!(job.trace_id, upload.trace_id);
    assert_eq!(hex(&job.parent_span_id), hex(&upload.span_id));
    assert_eq!(
        traceparent,
        format!("00-{}-{}-01", hex(&upload.trace_id), hex(&upload.span_id)),
        "the stored context names the span the backend has"
    );

    // A read's route is a template; the filename is a placeholder in it.
    assert!(
        spans.iter().any(|span| attribute(span, "http.route")
            .is_some_and(|route| route.ends_with("/original/{filename}"))),
        "{spans:#?}"
    );
}

#[tokio::test]
async fn otlp_over_http_and_sentry_send_what_they_should_and_no_more() {
    let db = TestDatabase::new().await;
    let (collector, received) = fake_endpoints().await;
    let exporters = |addr: SocketAddr| {
        format!(
            r#"
            [otel]
            endpoint = "http://{addr}"
            protocol = "http/protobuf"
            headers = {{ authorization = "Bearer collector-key" }}
            [sentry]
            dsn = "http://sentry-key@{addr}/1"
            environment = "test"
            traces_sample_rate = 1.0
            "#
        )
    };
    let mut server = Server::start(&db, exporters, collector).await;
    let (tokens, traceparent) = exercise(&server, &db).await;
    server.stop().await;

    assert_clean(&received, &tokens);
    assert_one_trace(&received, &traceparent);
    assert!(
        received
            .otlp_http
            .lock()
            .unwrap()
            .iter()
            .all(|(path, _)| path == "/v1/traces")
    );

    // The dead job is an event, tagged with the trace it belongs to, and
    // the transactions came too.
    let sentry = received.sentry_text();
    assert!(sentry.contains("job failed for the last time"), "{sentry}");
    assert!(sentry.contains("\"environment\":\"test\""), "{sentry}");
    assert!(sentry.contains("\"job_kind\":\"scan\""), "{sentry}");
    assert!(server.logged().contains("last_error="), "still in the logs");
    let trace = traceparent.split('-').nth(1).unwrap();
    assert!(
        sentry.contains(&format!("\"trace_id\":\"{trace}\"")),
        "{sentry}"
    );
    assert!(sentry.contains("\"type\":\"transaction\""), "{sentry}");
}

#[tokio::test]
async fn otlp_over_grpc_alone() {
    let db = TestDatabase::new().await;
    let (collector, received) = fake_endpoints().await;
    let exporters = |addr: SocketAddr| {
        format!(
            r#"
            [otel]
            endpoint = "http://{addr}"
            "#
        )
    };
    let mut server = Server::start(&db, exporters, collector).await;
    let (tokens, traceparent) = exercise(&server, &db).await;
    server.stop().await;

    assert_clean(&received, &tokens);
    assert_one_trace(&received, &traceparent);
    assert!(received.sentry.lock().unwrap().is_empty());
    assert!(received.otlp_http.lock().unwrap().is_empty());
}

/// Neither configured: nothing is sent, whatever happens, and the context
/// is the generated one the tests of `agrafe-axum` cover.
#[tokio::test]
async fn neither_configured_sends_nothing() {
    let db = TestDatabase::new().await;
    let (collector, received) = fake_endpoints().await;
    let mut server = Server::start(&db, |_| String::new(), collector).await;
    let (_, traceparent) = exercise(&server, &db).await;
    server.stop().await;
    assert!(received.everything().is_empty());
    assert!(traceparent.starts_with("00-"), "{traceparent}");
    assert!(Path::new(env!("CARGO_BIN_EXE_agrafe")).exists());
}

/// TLS to an endpoint that does not speak it, and to nothing at all. The
/// three clients each set up rustls their own way, in a tree that has two
/// of its providers: none may panic, a failing export costs the requests
/// nothing, and the process still stops in time.
#[tokio::test]
async fn a_collector_that_fails_costs_nothing() {
    for protocol in ["grpc", "http/protobuf"] {
        let db = TestDatabase::new().await;
        let (collector, received) = fake_endpoints().await;
        let exporters = |addr: SocketAddr| {
            format!(
                r#"
                [otel]
                endpoint = "https://{addr}"
                protocol = "{protocol}"
                [sentry]
                dsn = "https://sentry-key@{addr}/1"
                "#
            )
        };
        let mut server = Server::start(&db, exporters, collector).await;
        exercise(&server, &db).await;
        let started = Instant::now();
        server.stop().await;
        let logged = server.logged();
        assert!(!logged.contains("panicked"), "{logged}");
        assert!(started.elapsed() < Duration::from_secs(15), "{logged}");
        assert!(received.everything().is_empty());
    }
}
