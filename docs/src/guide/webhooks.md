# Webhooks

Instead of polling `GET /blobs/:id`, configure one URL and agrafe `POST`s to
it whenever a blob reaches a state your backend waits for.

```toml
[webhook]
url = "https://backend.example/agrafe/events"
secret = "whsec_…"   # echo "whsec_$(openssl rand -base64 24)", or AGRAFE_WEBHOOK__SECRET
# events = ["ready", "failed"]
# allow_insecure = true   # for an http URL, in development only
```

Nothing is enqueued while `url` is unset.

## Events

| Event | When | What to do |
|---|---|---|
| `ready` | The original became readable: scan passed or skipped, build finished, fetch done | Show the file, store the envelope |
| `infected` | The scan found something. Terminal | Tell the user, detach |
| `failed` | The job that was to finish the blob ran out of attempts | Retry with `POST /blobs/:id/retry`, or give up |
| `expired` | The reaper recorded the blob `expired` | In practice, an attached blob whose file never came |

There is no event for `deleted`, which the backend caused, nor for
representations, which have `wait` and the placeholder icon. `expired` is
sent when the reaper records the expiry, up to `jobs.reap_interval` after
the instant it passed. Attached blobs never expire, so the event is mostly
the signal of an abandoned upload.

`webhook.events` narrows the set.

## The request

```http
POST /agrafe/events HTTP/1.1
content-type: application/json
user-agent: agrafe/0.1.0
webhook-id: 019a1c2e-7f3b-7c41-9a0e-3f5d2b8c1e44
webhook-timestamp: 1792393200
webhook-signature: v1,K5oZfzN95Z9UVu1EsfQmfVNQhnkZ2pj9o9NDN/H/pI4=
traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01

{
  "event": "ready",
  "occurred_at": "2026-10-19T09:12:03.418Z",
  "attempt": 1,
  "blob": {"id": "019a…", "state": "ready", "attachments": [ … ], "urls": { … }, "envelope": "…", …}
}
```

- `event` is why you are being called.
- `occurred_at` is when the transition was recorded.
- `attempt` counts the deliveries of this event, this one included.
- `blob` is exactly what `GET /blobs/:id` answers at delivery time,
  attachments, URLs and envelope included.

## Apply the state, not the event

Delivery is at least once, and two deliveries for one blob can cross. The
body carries the blob as it is when the delivery is sent, so always apply
`blob.state` and `blob.updated_at`, and treat `event` as the reason for the
call.

For example, a `failed` delivery that is retried after `POST
/blobs/:id/retry` fixed the blob arrives with `blob.state` already `ready`.

One `notify` job is pending per blob. A second event raised while the first
is still undelivered replaces it, so the backend hears the newest news, not
a `ready` that is no longer true.

## Verifying the signature

The headers follow [Standard Webhooks](https://www.standardwebhooks.com/),
so any of its libraries verifies a delivery with the `whsec_` secret as is.

- `webhook-id` identifies the delivery. It is the same on every attempt of
  one event, so deduplicate on it.
- `webhook-timestamp` is the Unix time of sending.
- `webhook-signature` is `v1,` followed by the base64 of an HMAC-SHA256
  over `id.timestamp.body`, keyed with the base64-decoded part of the
  secret after `whsec_`.
- `traceparent` is the W3C trace context of the event: the trace of the
  request to agrafe that led to it, so a receiver that continues it ties
  the event to the upload in its own traces. It is not signed.

Verify over the raw body, before parsing it, then reject stale timestamps.
By hand, in Ruby:

```ruby
def verify!(secret, headers, raw_body)
  id, timestamp, signatures = headers.values_at("webhook-id", "webhook-timestamp", "webhook-signature")
  raise "stale" if (Time.now.to_i - timestamp.to_i).abs > 300
  key = Base64.strict_decode64(secret.delete_prefix("whsec_"))
  expected = "v1," + Base64.strict_encode64(OpenSSL::HMAC.digest("SHA256", key, "#{id}.#{timestamp}.#{raw_body}"))
  raise "bad signature" unless signatures.split(" ").any? { |s| Rack::Utils.secure_compare(s, expected) }
end
```

The webhook secret is its own secret, not a signing key: URL signing keys
should not double as a secret shared with a receiver.

## Delivery and retries

A `2xx` answer within `webhook.timeout`, ten seconds by default, is a
delivery. Anything else fails the attempt: another status, a timeout, a
connection error. Redirects are not followed.

Failed attempts are retried through the job queue, 30 seconds after the
first failure, doubling up to an hour, ten attempts over about three hours
in all. The delivery is
then a dead `notify` job, with the head of the receiver's answer in its
`last_error`:

```sh
curl -s 'localhost:8080/jobs?kind=notify&state=dead' \
  | jq '.items[] | {id, blob_id, last_error}'
curl -s -X POST "localhost:8080/jobs/$JOB_ID/retry"
```

A backend down for an hour drains its backlog by itself once it is back. A
dead `notify` says nothing on the blob: it is the news of the blob, not its
progress.

A delivery is enqueued in the same transaction as the state change it
reports, so there is never a transition without its notification, and never
a notification for a transition that did not happen.

## Requirements on the URL

The URL must be `https` unless `webhook.allow_insecure` is set: the body
carries a `url` blob's presigned source and the blob's envelope. There is
one URL per deployment, not one per blob: a per-blob callback URL would hand
an SSRF primitive to whoever reaches the private port.

## Monitoring deliveries

| Series | Meaning |
|---|---|
| `agrafe_webhook_deliveries_total{event,result}` | Attempts by this process, `delivered` or `failed` |
| `agrafe_jobs{kind="notify",state="dead"}` | Deliveries given up: alert on it |
| `agrafe_jobs_oldest_due_seconds{kind="notify"}` | How far behind deliveries are |

See [Monitoring](monitoring.md).
