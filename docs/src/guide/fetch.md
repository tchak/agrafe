# Fetching from a URL

A `url` blob is one agrafe downloads itself, instead of receiving it through
a `PUT`. The backend gives the URL at creation and sends no bytes. The first
use is migrating from another store, such as Active Storage on S3: one
`POST /blobs` per record with a presigned URL, and the files move between
the two stores without passing through the backend.

## Create a fetched blob

```sh
curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d '{
    "filename": "receipt.pdf",
    "content_type": "application/pdf",
    "input_type": "url",
    "url": "https://bucket.s3.example/abc?X-Amz-Signature=…",
    "checksum_md5": "kDU2JDbbJuRTYpXJ7p6T/Q==",
    "size": 183204,
    "priority": "low",
    "attachment": {"owner_kind": "expense", "owner_id": "42", "role": "receipt"}
  }'
```

The blob is created `created` with a `fetch` job enqueued in the same
transaction. The job downloads the response body through the same pipeline
as an upload: size, SHA-256 and MD5, type sniffing, inline scan, encryption
and storage. The blob then lands in `uploaded` or `ready` like any upload.

Checksums and the size are optional for a `url` blob:

| Field | For `url` |
|---|---|
| `checksum` (SHA-256) | Checked when given; computed and recorded otherwise |
| `checksum_md5` | Checked when given; computed and recorded otherwise |
| `size` | Enforced when given; `upload.max_size` is the cap otherwise. A `Content-Length` that disagrees is refused before the body is read |

Active Storage's `checksum` column is the base64 of the raw MD5 digest,
which is exactly what `checksum_md5` takes, so a record's value pastes
straight in. `size` takes the record's `byte_size`.

The response's `Content-Type` is ignored. The blob gets the declared
`content_type`, and the sniffer checks the bytes against it as for any
upload.

## Network policy

The private port is trusted, but a URL makes agrafe connect wherever it
points, so fetches follow a strict policy:

- `https` only, with no credentials in the URL;
- the host is resolved by agrafe, and loopback, private, link-local,
  shared and cloud metadata ranges are refused, IPv4 and IPv6, including an
  IPv4 address mapped into IPv6. A name that resolves to one refused address
  among several is refused whole, and the connection goes to the addresses
  that were checked, so DNS rebinding cannot swap them;
- a literal address in the URL is checked at creation, so a mistake is a
  `422` rather than a dead job;
- at most `fetch.max_redirects` (five) redirects, each through the same
  checks;
- a fixed `User-Agent`, no cookies, no proxy.

`fetch.allow_private_networks = true` lifts the scheme and address rules,
for local development and tests only.

## Timeouts

| Setting | Default | Bounds |
|---|---|---|
| `fetch.connect_timeout` | 10 s | The TCP connect and the TLS handshake |
| `fetch.read_timeout` | 30 s | Every wait for the next bytes: a remote that stalls fails here |
| `fetch.timeout` | 6 h | The whole response, as a backstop |

A large file on a slow link runs as long as it keeps flowing.

## Failures

Some failures are permanent and kill the job at once:

- a 4xx answer other than 408, 425 and 429;
- a checksum, size or type mismatch, or a body over the cap;
- a refused address, a refused redirect or too many redirects;
- a TLS failure;
- a URL refused by the policy.

Others are transient and retried with the usual backoff, 30 seconds doubling:

- a 5xx, 408, 425 or 429;
- a host that does not resolve, since a resolver's bad minute looks the
  same as a typo;
- a connect, read or whole-response timeout, or a reset;
- a body shorter than its `Content-Length`;
- a bucket or database error.

A `Retry-After` on 429 and 503 is honoured, up to `fetch.max_retry_after`
(fifteen minutes); a longer one is treated as permanent. With five attempts,
a fetch gives up after about eight minutes. That is intended: a presigned
URL expires, and a long retry window would mostly retry dead links.

Once the job is out of attempts, the blob stays `created` with `failed_at`
and `last_error` set. `last_error` never contains the URL: presigned URLs
are secrets, and agrafe strips them from errors and logs. The URL itself is
reported in private metadata only, and only until the fetch completes.

To try again, `POST /blobs/:id/retry` enqueues a new fetch. A day later the
presigned URL has usually expired, so creating a new blob with a fresh URL
is often the better repair.

## Running a migration

For each record of the old store:

1. `POST /blobs` with `input_type: "url"`, the presigned URL, `filename`,
   `content_type`, `checksum_md5` from the record's `checksum`, `size` from
   its `byte_size`, `skip_virus_scan` if you vouch for the source,
   `priority: "low"` so interactive uploads go first, and the `attachment`.
2. Take the `ready`, `failed` and `expired` events from the
   [webhook](webhooks.md), or poll.
3. Sweep the failures:

```sh
# Within a day of the failure: blobs whose fetch died
curl -s 'localhost:8080/blobs?failed=true' | jq '.items[] | {id, last_error}'

# A day after the failure: an attached blob whose fetch died is expired by
# the reaper, attachments kept, so the owner's side sees it
curl -s 'localhost:8080/attachments?owner_kind=expense&role=receipt' \
  | jq '.items[] | select(.blob_state == "expired")'
```

A fetch worker pool of its own (`jobs.concurrency.fetch`, two by default)
keeps a migration batch from taking scan or render workers. An attached blob
waits in the queue as long as the batch takes: only a fetch that died starts
the day. An unattached one expires at its `expires_at` whether it was fetched
or not, so a migration that attaches later sets an `expires_in` long enough
for the whole batch.
