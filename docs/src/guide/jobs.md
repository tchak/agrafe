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
advisory lock keeps two runs from overlapping.

## Where jobs run

`server.role`, or `serve --role`, picks what a process runs: `api`,
`worker` or `all`. `jobs.kinds`, or `serve --kinds`, narrows the kinds a
worker handles. See [Deployment](deployment.md#splitting-the-api-and-the-workers).
Every kind must be handled by some process, or its jobs pile up, which the
oldest-due metric shows.
