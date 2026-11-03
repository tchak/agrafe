# Monitoring

agrafe exposes four things to watch it by: Prometheus metrics, the jobs API,
the failure markers on blobs, and an HTML dashboard. All of them are on the
private port. It can also send its errors to Sentry and its spans to an
OpenTelemetry collector; see [Exporting errors and
traces](#exporting-errors-and-traces).

## Metrics

`GET /metrics` answers in the Prometheus text format. Every process serves
it on `server.private_addr`, worker-only and public-only ones included.

```yaml
# prometheus.yml
scrape_configs:
  - job_name: agrafe
    static_configs:
      - targets: ["agrafe-api:8080", "agrafe-worker-1:8080", "agrafe-worker-2:8080"]
```

### Queue gauges

These are read from the `jobs` table at scrape time, so **any one process
reports for the whole deployment**. Scrape them from one target, or
deduplicate them in queries, rather than summing across processes.

| Metric | Labels | Meaning |
|---|---|---|
| `agrafe_jobs` | `kind`, `state`, `priority` | Jobs in the queue. `state` is `due` (runnable now), `scheduled` (waiting for `run_at`: a retry backing off, a purge waiting for its delay), `running` (lease current) or `dead`. Exact up to a thousand per kind, priority and state; past that, PostgreSQL's estimate, its order of magnitude |
| `agrafe_jobs_oldest_due_seconds` | `kind`, `priority` | How long the oldest due job has been waiting |
| `agrafe_jobs_census_ok` | | 1 when the two above come from this scrape, 0 when the census failed (the database out of reach or too slow to answer in five seconds) and they are the last scrape's |

A job whose lease lapsed counts as due, as the claim query sees it.

### Process counters

These count what one process did, so sum them across processes.

| Metric | Labels | Meaning |
|---|---|---|
| `agrafe_job_runs_total` | `kind`, `outcome` | Finished attempts. `outcome` is `ok`, `retry`, `dead` or `superseded` (dropped in favour of a newer twin) |
| `agrafe_job_duration_seconds` | `kind` | Histogram of attempt wall time, buckets from 50 ms to one hour |
| `agrafe_workers` | `kind` | Workers this process runs, zero for a kind it does not, so a kind no process handles sums to zero |
| `agrafe_blob_head_cache_total` | `result` | Row lookups by the read routes, `hit` or `miss` |
| `agrafe_webhook_deliveries_total` | `event`, `result` | Webhook attempts, `delivered` or `failed` |

### The audit

The last audit this process ran, see [Jobs and
retries](jobs.md#the-audit). Gauges, not counters: a sweep is a census.
Only the process whose worker ran the audit has values; the others report
zero.

| Metric | Labels | Meaning |
|---|---|---|
| `agrafe_audit_findings` | `kind` | Mismatches found, by kind: `missing_original`, `damaged_original`, `missing_representation`, `missing_source`, `orphan`, `unknown`, `foreign` |
| `agrafe_audit_objects` | | Objects the last audit listed in the bucket |
| `agrafe_audit_blobs` | | Blobs the last audit checked |
| `agrafe_audit_errors` | | Findings the last audit could not act on: a stamp, a render request, a delete or a read back that failed; the next run finds them again |
| `agrafe_audit_last_run_timestamp_seconds` | | When the last audit finished, as a Unix time; zero until one has run |

## What to alert on

**The oldest due job** is the number that tells whether a kind keeps up. It
grows when a kind's workers cannot keep up, when no process handles the
kind, or, for one priority alone, when higher priorities starve it.

These rules are `config/alerts.yml` in the repository, and
`config/alerts.test.yml` beside it runs them with `promtool test rules`, which
is how to check the changes you make to them:

```yaml
{{#include ../../../config/alerts.yml}}
```

The audit gauges are per process and start at zero, so these two rules read
them with that in mind:

- `AgrafeAuditStale` looks over the past 15 days, so a restart does not fire
  it, and stays quiet until the deployment has been scraped for two weeks,
  since the first sweep starts a week after the first start. It needs that
  much retention, which is Prometheus' default. A run starts a week after the
  last one ended and the gauge moves when it finishes, so the threshold is a
  week plus the longest sweep you expect: raise it if yours take over a week.
- `AgrafeAuditFindings` resolves when the process that ran the audit restarts,
  though nothing was repaired. The stamped blobs stay in `GET
  /blobs?failed=true`, which is where to check.

A sweep that dies does not wait for either: it is a dead `audit` job, which
`AgrafeDeadJobs` reports. A dead job does not end the weekly cadence, so
`AgrafeAuditStale` catches what is left, sweeps that never finish.

A missing original is an incident: the bytes cannot be recovered from
agrafe's side, and the blob is listed under `GET /blobs?failed=true` with
`audit:` at the head of its `last_error`. Orphans only cost storage; look
at them before the next restore, with `agrafe audit --fix` once you know
what they are. Unknown prefixes, objects no row has, are rows lost, which
is what a database restored from an older backup leaves: the audit's log
line says how many prefixes and when their blobs were created, which is
the span to recover. `--fix` leaves them alone; `--delete-unknown` is the
flag that deletes them, for once the rows are not coming back.

Thresholds depend on your traffic. Scans and renders normally run within
seconds of the upload. A large archive can legitimately take hours, and
only one is built per process at a time by default.

Other signals worth a graph:

- `rate(agrafe_job_runs_total{outcome="retry"}[5m])` by kind: a rising
  retry rate on `scan` usually means `clamd` is down; on `fetch`, a remote
  is failing.
- `histogram_quantile(0.95, sum by (le, kind) (rate(agrafe_job_duration_seconds_bucket[15m])))`:
  render times creeping towards `media.sandbox.timeout` mean files are
  timing out.
- The cache hit ratio, `agrafe_blob_head_cache_total{result="hit"}` over
  the total, shows how much the read path relies on the database. Sealed
  URLs skip the lookup entirely and are not counted.

## The jobs API

The queue is inspectable as JSON:

```sh
# Dead jobs, newest first
curl -s 'localhost:8080/jobs?state=dead' | jq '.items[] | {id, kind, blob_id, attempts, last_error}'

# One blob's jobs
curl -s "localhost:8080/jobs?blob_id=$ID" | jq

# Give a dead job its attempts back, or discard it
curl -s -X POST "localhost:8080/jobs/$JOB_ID/retry"
curl -s -X DELETE "localhost:8080/jobs/$JOB_ID"
```

Filters are `kind`, `state`, `priority` and `blob_id`. See the [private API
reference](../reference/private-api.md#jobs).

## Failed blobs

Dead jobs are pruned after `jobs.keep_failed`, seven days by default. The
blobs they left stuck keep the verdict as `failed_at` and `last_error`, so
the backend's view does not depend on the operator's:

```sh
curl -s 'localhost:8080/blobs?failed=true' | jq '.items[] | {id, input_type, state, last_error}'
curl -s -X POST "localhost:8080/blobs/$ID/retry"
```

Common `last_error` contents and what they mean:

| Kind | Typical error | Likely cause |
|---|---|---|
| `scan` | connection refused | `clamd` is down or unreachable |
| `scan` | an `ERROR` reply | `StreamMaxLength` is below the file's size |
| `fetch` | an HTTP status | The presigned URL expired (`403`) or the object is gone (`404`) |
| `render` | an object store error | The bucket is unreachable; renders retry by themselves |
| `document` | `main.typ:12:5: error: …` | The template does not compile with this data |
| `notify` | a status and the head of the body | The backend's receiver refused the delivery |

## The dashboard

The dashboard shows the same things as HTML pages, in the spirit of
Sidekiq's web UI. It is off by default.

```toml
[dashboard]
enabled = true   # /dashboard on the private port
actions = true   # the retry and discard buttons; off, the pages are read-only

[server]
private_hosts = ["agrafe.internal:8080"]
```

- **Jobs:** the queue census, a total per state and, per kind and priority,
  the count in each state (`≈` marks an estimate, past a thousand) and the
  age of the oldest due job; then the
  listing, with a tab per state and the `kind`, `priority` and `blob_id`
  filters. A job's page shows its payload and its whole error, and, when it
  is dead, the retry and discard buttons. `?refresh=5` reloads the page every
  five seconds, the shortest interval it takes.
- **Blobs:** the filters of `GET /blobs`. A blob's page shows its
  thumbnail, its attachments and its jobs, and a retry button when it has
  failed.
- **Attachments:** the filters of `GET /attachments`, each row with the
  state and failure of its blob.

Every row links to the related objects, and the URL holds the filters, so a
view can be pasted to a colleague. There is no JavaScript. The dashboard is
served by `api` and `all` processes only.

It has no authentication of its own. Put it behind a proxy that has some,
and set `server.private_hosts`, which agrafe warns about at startup when it
is missing. See [Security model](security.md#browsers-on-the-private-port).

## Health checks

`GET /healthz` answers `200 ok` on both ports, and on the private address of
every process that serves no private API, worker-only and public-only
alike. It says the process is up; it does not check
Postgres, the bucket or `clamd`, whose failures show in the job metrics
instead.

## Logs

agrafe logs to standard error through `tracing`, filtered by `RUST_LOG`;
standard output is left to what a command reports, the tally of `agrafe
audit`. A directive `RUST_LOG` cannot parse is named on standard error at
startup and left out, the others kept:

```sh
RUST_LOG=info agrafe serve                 # default
RUST_LOG=agrafe=debug,info agrafe serve    # agrafe's own debug output
```

Internal errors are logged in full with the `internal error` message the
client receives. Every request runs in a `request` span naming its method
and path, and every job attempt in a `job` span naming its kind and id:
a success logs `job done` at `info`, a failure to be retried `job failed`
at `warn`, and the last attempt `job failed for the last time` at `error`,
which is the line to route to an alert channel.
Public request paths are logged with the token redacted, and the HTTP client
crates stay at `info` unless named explicitly, so presigned fetch URLs never
reach the logs.

### Following a request to its jobs

Both spans carry a `trace_id`, and a job has the `trace_id` of the request
or the job that enqueued it. Grep the logs for one id to see an upload, its
scan, the render the scan led to and the webhook that reported the result:

```text
INFO request{method=PUT path=/blobs/0199f6a0-… trace_id=4bf92f35…}: …
INFO job{id=1842 kind=scan … trace_id=4bf92f35…}: agrafe::jobs::runner: job done
INFO job{id=1843 kind=render … trace_id=4bf92f35…}: agrafe::jobs::runner: job done
```

The jobs API and the dashboard's job page show a job's `trace_id` too, so a
dead job leads back to the request that caused it.

A request on the private port continues the trace of a W3C `traceparent`
header when it sends one, so a backend that propagates its own trace
context gets the same id in agrafe's logs as in its own. Without the
header, or with a malformed one, the request starts a new trace. The public
port always starts a new trace: its callers are browsers, and the id is not
theirs to pick. Jobs nothing traced, such as the periodic reaper and audit,
start their own.

Inside those spans, the slow operations have spans of their own: `storage`
for each object store call, with `op` and `path`; `sandbox` for each
renderer or helper run, with the `program` and its `outcome`; and `clamav`
for the connection and the wait for the verdict. They show in a log line's
context when something is logged inside them. Nothing is exported yet; the
spans and the stored trace context are what an OpenTelemetry or Sentry
exporter will build on.

## Exporting errors and traces

Two optional exporters, independent of each other and both off until
configured, in `[sentry]` and `[otel]`; see the [configuration
reference](../reference/configuration.md#sentry). They are in the `agrafe`
binary and the release images, not in the library: a host embedding agrafe
brings its own.

**Sentry** gets errors: each `error` log line becomes an event, grouped into
issues with alerting and resolve and regress tracking per release, the `info`
and `warn` lines before it as breadcrumbs, and panics, which here fail one job
attempt or drop one connection rather than crash the process.

```toml
[sentry]
dsn = "https://key@sentry.example/12"
environment = "production"
```

**OpenTelemetry** gets spans, over OTLP to Tempo, Jaeger, SigNoz or anything
else that speaks it, Sentry included. Metrics stay on `/metrics` and logs on
standard error.

```toml
[otel]
endpoint = "http://collector:4317"
sample_ratio = 0.1
```

A request and the jobs it leads to are one trace, the job's span the child of
the request's or job's span that enqueued it, and the `trace_id` in the logs,
on a job in the jobs API and on a Sentry event as a tag is the backend's. The
sampling decision is made where a trace starts and goes down it, through the
stored job, so a trace is kept whole or not at all. A span's name is the route
template, `GET /blobs/{token}/{id}/original/{filename}`, or `job scan`.

A failing collector costs requests nothing: spans wait in a bounded queue, and
what does not fit is dropped. Both are said in the logs, as `warn` and `error`
lines under `opentelemetry_sdk` (`BatchSpanProcessor.ExportError`,
`BatchSpanProcessor.SpanDroppingStarted`): the exporters connect lazily, so a
wrong endpoint or key is found there, not at start. Sentry's SDK is silent
about a failed delivery unless `sentry.debug` is on, which prints its
diagnostics on standard error. On a stop, the process gives what is left five
seconds. A periodic reap or audit is one trace with everything it enqueues,
thousands of spans on a large table; lower `sample_ratio` if the backend
minds.

### What stays in the process

- **Signatures.** The token of a public URL and the query string, where a
  sealed read's envelope is, are taken out of every string of every span,
  event and breadcrumb before it leaves, whichever field they landed in. A
  span records `http.route`, the template, and not the path.
- **Filenames.** The template has a placeholder where a path has the name, and
  the `path` field is not exported.
- **What a job's failure quotes.** `last_error` can hold a compiler's
  diagnostics about a user's document or a tool's standard error. It is not
  exported: the event says that a job of that kind died, and its `trace_id`
  leads to the logs and the job's page, which have the message.
- **Presigned URLs.** The HTTP client crates are capped at `info` for the
  exporters, unconditionally: `RUST_LOG=hyper=debug` opens them for the logs
  alone.
- **Addresses and variables.** No client IP or user is attached, nor any
  stack variable; `attach_stacktrace` adds frames only.

Other `error` lines, an `internal error` above all, keep their message, which
holds the cause: a database or object store error, naming ids rather than
content.

`RUST_LOG` is the logs' filter alone, so `RUST_LOG=warn` does not thin the
exporters. Only `AGRAFE_SENTRY__DSN` and `AGRAFE_OTEL__ENDPOINT` turn an
exporter on, as everywhere else in the configuration; `SENTRY_DSN` and
`OTEL_EXPORTER_OTLP_ENDPOINT` do not. Once on, the SDKs still read their other
variables for what agrafe does not set, such as `OTEL_EXPORTER_OTLP_HEADERS`.

