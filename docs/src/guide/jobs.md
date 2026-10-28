# Jobs and retries

Everything that happens after a request runs as a job: fetching, deferred
scanning, rendering, building, purging, delivering webhooks and the periodic
reaper. Jobs live in a Postgres table, so there is no broker to run. A job is
enqueued in the same transaction as the state change that calls for it.

## Kinds

| Kind | Does | Workers per process |
|---|---|---|
| `fetch` | Downloads a `url` blob | 2 |
| `scan` | Streams a stored blob to `clamd` | 4 |
| `render` | Renders the thumbnail and the preview | 4 |
| `rows` | Builds a spreadsheet | 2 |
| `archive` | Builds a ZIP | 1 |
| `document` | Compiles a PDF | 2 |
| `purge` | Deletes a terminal blob's objects | 4 |
| `reap` | Periodic housekeeping | 1 |
| `notify` | Delivers a webhook | 4 |
| `audit` | Weekly sweep of the bucket against the `blobs` table | 1 |

Each kind has its own pool, sized by `jobs.concurrency`. However many slow
jobs of one kind are queued, the others keep flowing. There is no priority
across kinds.

## Priority

A blob's `priority`, `high`, `normal` or `low`, is copied onto its fetch,
scan, render, build and notify jobs, and orders them within their kind.
Purges and the reaper always run `normal`. Priority orders, it does not
reserve: a kind fed `high` jobs without end never runs its `low` ones, which
the per-priority oldest-due metric shows.

Use it to let an interactive upload jump a bulk import: create migration
fetches `low` and user exports `high`.

## Lifecycle

A worker claims the highest-priority, then oldest, due job of its kind, and
holds a lease on it for `jobs.lease_timeout`, ten minutes, renewed every
third of that while the job runs. A worker that dies lets its lease lapse,
and the job is claimed again.

- **Success** deletes the job.
- **Failure** schedules a retry with exponential backoff: 30 seconds,
  doubling, capped at an hour, or longer when a remote asked for it with
  `Retry-After`.
- After five attempts the job is **dead**: never run again, kept with its
  last error for `jobs.keep_failed`, seven days by default.

A failure that another attempt would repeat kills the job at once: a
template that does not compile, a fetch answered `404`, a source line that
does not parse.

Job states, as the API reports them:

| State | Meaning |
|---|---|
| `due` | Unclaimed and runnable now |
| `scheduled` | Unclaimed, `run_at` ahead: a retry backing off, a purge waiting for its delay |
| `running` | Claimed, lease current |
| `dead` | Out of attempts |

## Failed blobs

A dead job that was to finish its blob, a fetch, a scan, a build or a
render, leaves its verdict on the blob too: `failed_at` and `last_error` in
the metadata, `blob_failed_at` in the attachments listing, and a `failed`
webhook event. The dead job itself is pruned after a week; the blob's copy
stays as long as the blob is stuck.

```sh
curl -s 'localhost:8080/blobs?failed=true' | jq '.items[] | {id, state, last_error}'
```

### Retrying from the blob

```sh
curl -s -X POST "localhost:8080/blobs/$ID/retry" | jq .state
```

`POST /blobs/:id/retry` enqueues whatever the blob is waiting on, at the
blob's priority, deletes the dead jobs it replaces, clears the failure, and
answers `202` with the metadata.

| Blob | Enqueues |
|---|---|
| `created`, `url` input | a fetch |
| `uploaded` | the scan, or the build of a generated blob |
| `ready` with a representation `pending` | a render |

It answers `409` when the blob waits on nothing, when the job it waits on is
already pending or running, or when the blob is `infected`, and `410` for
an expired or deleted blob. It needs no job id, so it works after the dead
job was pruned. It is the only way to restart a render whose job died.

### Retrying a job

The operator's side works on jobs directly:

```sh
curl -s 'localhost:8080/jobs?state=dead' | jq '.items[] | {id, kind, blob_id, last_error}'
curl -s -X POST "localhost:8080/jobs/$JOB_ID/retry"   # attempts reset, runs now
curl -s -X DELETE "localhost:8080/jobs/$JOB_ID"       # discard
```

Both answer `409` for a job that is not dead, and a retry answers `409`
when the blob already has a pending job of that kind.

## The reaper

The `reap` job runs every `jobs.reap_interval`, one minute by default. Each
run, in one transaction:

1. records `expired` on every non-terminal blob past its `expires_at`;
2. expires abandoned blobs: unfinished sources, and attached `created` blobs
   untouched for `upload.abandoned_after`;
3. prunes dead jobs older than `jobs.keep_failed`;
4. schedules a purge, `upload.purge_delay` out, for every terminal blob that
   still holds objects;
5. hands uploads stale for `upload.stale_after` back to `created`;
6. prunes tombstones purged more than `blobs.keep_purged` ago, with their
   attachments and jobs.

Expiry itself never waits for the reaper: reads compare `expires_at` with
the clock. One reap is pending at a time across the deployment, and an
advisory lock keeps two runs from overlapping. The lock is keyed on the
database and schema, so several deployments sharing one Postgres server
do not get in each other's way.

## The audit

The `audit` job checks that the bucket and the `blobs` table agree, every
`jobs.audit_interval`, seven days by default, the first run one interval
after the workers start and each one scheduling the next when it is done.
It is for the mismatches nothing else sees: a
bucket or a database restored from backup, a lifecycle rule, a bad purge.
It merges the bucket listing, one request per thousand objects, with the
blobs table, one query per thousand rows, both in key order; nothing is
held for the length of the bucket. The listing's sizes are enough to
catch a truncated object; nothing is decrypted.

What it finds, one `warn` line per finding and a tally in the metrics:

| Finding | What it is | What the job does |
|---|---|---|
| `missing_original` | A blob that is not expired, deleted or infected has no `original` object | Stamps `failed_at` and `last_error` on the blob, so it shows in `GET /blobs?failed=true` |
| `damaged_original` | The original has a length its size does not produce (or, under `--deep`, does not decrypt to its checksum) | The same stamp |
| `missing_representation` | A thumbnail or preview the row says is `ready` is missing or the wrong length | Sets it back to `pending` and enqueues the render |
| `missing_source` | A source part a generated blob still needs is missing or truncated | Reports; the build fails on it itself |
| `orphan` | An object under a known blob that its row does not account for: a purged blob's leftovers, an original under a row that never recorded it, a representation the row says is not there | Reports |
| `unknown` | An object under a prefix no row has: a purge's leftover whose tombstone is gone, or a blob whose row was lost, which is what a database restored from an older backup leaves | Reports, with how many such prefixes and when their blobs were created |
| `foreign` | A key that is not `<shard>/<blob id>/<name>` | Reports |

What the listing does not show, the audit does not see: a multipart
upload a crashed process left open on S3 or GCS is parts, not an object,
and only the bucket's lifecycle rule reclaims them, see
[Deployment](deployment.md#object-storage).

A row that changed within `upload.stale_after` of the listing passing it
is left for the next run and counted as unsettled: an upload or a render
finishing as the audit went by may have written an object after the
listing looked, and the listing is not evidence against it. An object
that young which no row accounts for is left alone the same way, its
row may not say so yet.

The job never deletes. Deleting is `agrafe audit --fix` on the command
line, the operator with the report in front of them, and even then only
orphans older than `upload.stale_after`, so an upload or a render that
has written its object and not yet updated its row is left alone. A blob
awaiting its purge is skipped: its prefix is the purge's.

`--fix` never touches a prefix no row has. Those are `unknown`, and the
tally says how many prefixes and when their blobs were created, from
their ids. After a database restored from backup, that span is what the
backup missed: the objects are still the files, the backend still holds
the envelopes it minted sealed URLs from, and the rows are what to
recover, from a later backup or from what the backend kept. Deleting
them takes `--delete-unknown`, a flag of its own; do not run it until the
database is as recovered as it will be.

```sh
agrafe --config agrafe.toml audit            # report
agrafe --config agrafe.toml audit --fix      # and delete the old orphans
agrafe --config agrafe.toml audit --delete-unknown   # and the prefixes no row has
agrafe --config agrafe.toml audit --deep     # and decrypt every original
agrafe --config agrafe.toml audit --deep --sample 100   # one in a hundred
```

A sweep of a large bucket takes days, and the job keeps its place: every
minute it saves the last key listed, the last row passed and the tally so
far into its payload (visible in `GET /jobs`), and the attempt that finds
them there carries on from that point, after a restart that let the lease
lapse or a failure the runner retries; a save gives the job its attempts
back, so only failing repeatedly without getting anywhere kills it.
Acting on a finding never stops the sweep: a stamp, a render request or
a delete that fails is logged at `error`, counted as not acted on, and
the next run finds it again.

The command prints the tally and exits non-zero when anything was found
or could not be acted on; foreign objects do not count, so a README in
the bucket does not fail a restore procedure. Under `--fix` the orphans
go a thousand per request on S3. One audit runs at a time: the command reports
a running job, and a job that finds the command running steps aside
until its next interval. See
[Command line](../reference/cli.md#agrafe-audit) and
[Monitoring](monitoring.md#the-audit).

## Where jobs run

`server.role`, or `serve --role`, picks what a process runs: `api`,
`worker` or `all`. `jobs.kinds`, or `serve --kinds`, narrows the kinds a
worker handles. See [Deployment](deployment.md#splitting-the-api-and-the-workers).
Every kind must be handled by some process, or its jobs pile up, which the
oldest-due metric shows.
