# Monitoring

agrafe exposes four things to watch it by: Prometheus metrics, the jobs API,
the failure markers on blobs, and an HTML dashboard. All of them are on the
private port.

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
| `agrafe_jobs` | `kind`, `state`, `priority` | Jobs in the queue. `state` is `due` (runnable now), `scheduled` (waiting for `run_at`: a retry backing off, a purge waiting for its delay), `running` (lease current) or `dead` |
| `agrafe_jobs_oldest_due_seconds` | `kind`, `priority` | How long the oldest due job has been waiting |
| `agrafe_jobs_census_ok` | | 1 when the two above come from this scrape, 0 when the census failed (the database out of reach, or a backlog too long to count in five seconds) and they are the last scrape's |

A job whose lease lapsed counts as due, as the claim query sees it.

### Process counters

These count what one process did, so sum them across processes.

| Metric | Labels | Meaning |
|---|---|---|
| `agrafe_job_runs_total` | `kind`, `outcome` | Finished attempts. `outcome` is `ok`, `retry`, `dead` or `superseded` (dropped in favour of a newer twin) |
| `agrafe_job_duration_seconds` | `kind` | Histogram of attempt wall time, buckets from 50 ms to one hour |
| `agrafe_workers` | `kind` | Workers this process runs |
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

```yaml
groups:
  - name: agrafe
    rules:
      - alert: AgrafeQueueBehind
        expr: max by (kind, priority) (agrafe_jobs_oldest_due_seconds{kind!~"archive|rows|document"}) > 300
        for: 10m
        annotations:
          summary: "agrafe {{ $labels.kind }} jobs ({{ $labels.priority }}) waiting for over 5 minutes"

      - alert: AgrafeExportsBehind
        expr: max by (kind, priority) (agrafe_jobs_oldest_due_seconds{kind=~"archive|rows|document"}) > 3600
        for: 15m

      - alert: AgrafeDeadJobs
        expr: sum by (kind) (max without (instance) (agrafe_jobs{state="dead"})) > 0
        for: 5m
        annotations:
          summary: "agrafe has dead {{ $labels.kind }} jobs: GET /jobs?state=dead"

      - alert: AgrafeWebhookUndelivered
        expr: max without (instance) (agrafe_jobs{kind="notify", state="dead"}) > 0
        annotations:
          summary: "agrafe gave up delivering webhooks: GET /jobs?kind=notify&state=dead"

      - alert: AgrafeNoWorkers
        expr: sum by (kind) (agrafe_workers) == 0
        for: 5m

      - alert: AgrafeAuditFindings
        expr: sum by (kind) (agrafe_audit_findings{kind!~"orphan|foreign"}) > 0
        annotations:
          summary: "the last agrafe audit found {{ $labels.kind }} mismatches: GET /blobs?failed=true, or the audit log lines"

      - alert: AgrafeAuditStale
        expr: time() - max(agrafe_audit_last_run_timestamp_seconds) > 8 * 86400
        annotations:
          summary: "no agrafe audit has run for over eight days"
```

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
  the count in each state and the age of the oldest due job; then the
  listing, with a tab per state and the `kind`, `priority` and `blob_id`
  filters. A job's page shows its payload and its whole error, and, when it
  is dead, the retry and discard buttons. `?refresh=5` reloads the page every
  five seconds.
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

agrafe logs to standard output through `tracing`, filtered by `RUST_LOG`:

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
