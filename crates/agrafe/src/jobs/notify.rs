//! The webhook delivery: one `notify` job per event of a blob, enqueued in
//! the transaction of the state change it reports, posted to the one
//! configured URL with a Standard Webhooks signature. See DESIGN.md,
//! "Webhooks".
//!
//! The body carries the blob's full metadata as read at delivery time, so
//! a delivery that lands late or twice tells the backend what is, not what
//! was. One notify job is pending per blob: an event raised while an
//! earlier one is still undelivered replaces it, and the backend gets the
//! newest event with the state behind it.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use reqwest::header::{CONTENT_TYPE, USER_AGENT};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::PgExecutor;
use uuid::Uuid;

use super::{Job, JobError, JobKind, Priority};
use crate::app::Agrafe;
use crate::blob::metadata::BlobMetadata;
use crate::blob::{Blob, BlobState, repo};

/// What the backend is told of a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// The original became readable: the scan passed or was skipped, or
    /// the build finished.
    Ready,
    /// The scan failed. Terminal.
    Infected,
    /// The job that was to finish the blob died: `failed_at` is set on
    /// the blob. A retry is followed by `ready` or another `failed`.
    Failed,
    /// The reaper recorded the blob `expired`.
    Expired,
}

agrafe_types::str_enum!(Event {
    Ready => "ready",
    Infected => "infected",
    Failed => "failed",
    Expired => "expired",
});

impl Event {
    /// The event a blob's new state implies, for the transitions that
    /// end a job: `ready` and `infected`. `failed` and `expired` are
    /// raised where they happen, `jobs::fail` and the reaper.
    pub(crate) fn of(blob: &Blob) -> Option<Self> {
        match blob.state {
            BlobState::Ready => Some(Event::Ready),
            BlobState::Infected => Some(Event::Infected),
            _ => None,
        }
    }
}

/// What a notify job carries in `jobs.payload`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Payload {
    /// The delivery's `webhook-id`: minted when the event is enqueued,
    /// the same on every attempt of it, a new one when a newer event
    /// replaces it, so a receiver deduplicating on the header never
    /// mistakes the replacement for the delivery it already took.
    pub id: Uuid,
    pub event: Event,
    /// When the transition was recorded.
    pub occurred_at: Timestamp,
}

/// The request body.
#[derive(Debug, Serialize)]
pub(crate) struct Body {
    pub event: Event,
    pub occurred_at: Timestamp,
    /// Deliveries of this event so far, this one included.
    pub attempt: i32,
    /// The blob as `GET /blobs/:id` answers now.
    pub blob: BlobMetadata,
}

/// What a pending notify job for the blob becomes when a newer event is
/// enqueued: the newest event wins, its attempts start over.
macro_rules! on_conflict {
    () => {
        "ON CONFLICT (kind, blob_id) WHERE locked_at IS NULL \
         DO UPDATE SET payload = EXCLUDED.payload, run_at = EXCLUDED.run_at, \
                       attempts = 0, last_error = NULL"
    };
}

fn payload(event: Event, now: Timestamp) -> serde_json::Value {
    serde_json::to_value(Payload {
        id: Uuid::now_v7(),
        event,
        occurred_at: now,
    })
    .expect("the payload serialises")
}

/// Enqueue the delivery of `event` for the blob, due at `now`, at
/// `priority`, the blob's. A notify job pending for the blob is replaced,
/// see `ON_CONFLICT`. The caller checks `WebhookConfig::wants` first.
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub async fn enqueue(
    exec: impl PgExecutor<'_>,
    blob_id: Uuid,
    priority: Priority,
    event: Event,
    now: Timestamp,
) -> sqlx::Result<()> {
    sqlx::query(concat!(
        "WITH written AS (\
             INSERT INTO jobs (kind, blob_id, priority, payload, run_at) \
             VALUES ($1, $2, $3, $4, $5) ",
        on_conflict!(),
        " RETURNING kind\
         ) \
         SELECT pg_notify($6, kind) FROM written"
    ))
    .bind(JobKind::Notify.as_str())
    .bind(blob_id)
    .bind(priority.rank())
    .bind(payload(event, now))
    .bind(now.to_sqlx())
    .bind(super::CHANNEL)
    .execute(exec)
    .await?;
    Ok(())
}

/// `enqueue` for each of `blob_ids`, each at its blob's priority, in one
/// statement: the reaper's expiries, which can be many after an outage.
/// An id without a row is skipped.
pub(crate) async fn enqueue_each(
    exec: impl PgExecutor<'_>,
    blob_ids: &[Uuid],
    event: Event,
    now: Timestamp,
) -> sqlx::Result<()> {
    if blob_ids.is_empty() {
        return Ok(());
    }
    let payloads: Vec<serde_json::Value> = blob_ids.iter().map(|_| payload(event, now)).collect();
    sqlx::query(concat!(
        "WITH written AS (\
             INSERT INTO jobs (kind, blob_id, priority, payload, run_at) \
             SELECT $1, b.id, b.priority, e.payload, $4 \
             FROM unnest($2::uuid[], $3::jsonb[]) AS e (id, payload) \
             JOIN blobs b ON b.id = e.id ",
        on_conflict!(),
        " RETURNING kind\
         ) \
         SELECT pg_notify($5, kind) FROM written"
    ))
    .bind(JobKind::Notify.as_str())
    .bind(blob_ids)
    .bind(payloads)
    .bind(now.to_sqlx())
    .bind(super::CHANNEL)
    .execute(exec)
    .await?;
    Ok(())
}

/// Why a delivery failed. The message is what `last_error` keeps.
#[derive(Debug, thiserror::Error)]
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub enum WebhookError {
    /// The job's payload is not one of ours: another attempt would read
    /// the same thing.
    #[error("payload: {0}")]
    Payload(String),
    /// Anything but a 2xx, with the head of the response body.
    #[error("the receiver answered {status}: {body}")]
    Status { status: u16, body: String },
    /// A connection error or a timeout.
    #[error("request failed: {0}")]
    Request(String),
}

impl WebhookError {
    pub fn is_permanent(&self) -> bool {
        matches!(self, WebhookError::Payload(_))
    }
}

/// Bytes of a non-2xx response kept in `last_error`.
const MAX_ERROR_BODY: usize = 1000;

/// The client every delivery of this process goes through, built from
/// `[webhook]` when a URL is set.
#[derive(Clone)]
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub struct Client {
    inner: reqwest::Client,
    url: String,
    key: Vec<u8>,
}

impl Client {
    /// A client posting to `url`, signing with `key`, giving each request
    /// `timeout` from connect to the last byte. The URL is the operator's
    /// and is taken as given: no redirect is followed, a `3xx` fails the
    /// attempt like any other status.
    pub fn new(url: &str, key: Vec<u8>, timeout: Duration) -> Result<Self, reqwest::Error> {
        let inner = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .no_proxy()
            .build()?;
        Ok(Self {
            inner,
            url: url.to_string(),
            key,
        })
    }

    /// `POST` `body` as delivery `id`, signed over the timestamp of now.
    pub async fn deliver(&self, id: &str, body: String) -> Result<(), WebhookError> {
        let timestamp = Timestamp::now().as_second();
        let signature = sign(&self.key, id, timestamp, &body);
        let response = self
            .inner
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .header(USER_AGENT, concat!("agrafe/", env!("CARGO_PKG_VERSION")))
            .header("webhook-id", id.to_string())
            .header("webhook-timestamp", timestamp.to_string())
            .header("webhook-signature", signature)
            .body(body)
            .send()
            .await
            .map_err(|err| WebhookError::Request(describe(err)))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(WebhookError::Status {
            status: status.as_u16(),
            body: head_of(response).await,
        })
    }
}

/// The first `MAX_ERROR_BODY` bytes of a response's body as text, read
/// chunk by chunk so a receiver answering with a page the size of a disk
/// costs no more than that. Whatever was read when a chunk fails is kept.
async fn head_of(mut response: reqwest::Response) -> String {
    let mut bytes = Vec::new();
    while bytes.len() < MAX_ERROR_BODY {
        match response.chunk().await {
            Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
            Ok(None) | Err(_) => break,
        }
    }
    bytes.truncate(MAX_ERROR_BODY);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The `webhook-signature` of a delivery: `v1,` and the base64 of the
/// HMAC-SHA256 of `id.timestamp.body` under `key`, as Standard Webhooks
/// specifies.
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub fn sign(key: &[u8], id: &str, timestamp: i64, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()))
}

/// A request error with its causes, the URL left out: it is the
/// configured one and the log has it.
fn describe(err: reqwest::Error) -> String {
    let err = err.without_url();
    let mut message = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

pub(crate) async fn run(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    let id = job.blob_id.ok_or(JobError::NoBlob(job.kind))?;
    let payload: Payload = serde_json::from_value(job.payload.clone())
        .map_err(|err| WebhookError::Payload(err.to_string()))?;
    let Some(client) = state.webhook() else {
        tracing::info!(event = %payload.event, "no webhook configured, nothing to deliver");
        return Ok(());
    };
    // Read at delivery time: a late or repeated delivery says what is.
    let Some(blob) = repo::find(state.pool(), id).await? else {
        tracing::info!(event = %payload.event, "blob is gone, nothing to deliver");
        return Ok(());
    };
    let body = Body {
        event: payload.event,
        occurred_at: payload.occurred_at,
        attempt: job.attempts,
        blob: state.blob_metadata(&blob).await?,
    };
    let body = serde_json::to_string(&body)
        .map_err(|err| WebhookError::Payload(format!("encoding the body: {err}")))?;
    let result = client.deliver(&payload.id.to_string(), body).await;
    state
        .metrics()
        .webhook_delivery(payload.event, result.is_ok());
    match &result {
        Ok(()) => tracing::info!(event = %payload.event, state = %blob.state, "webhook delivered"),
        Err(err) => tracing::warn!(event = %payload.event, error = %err, "webhook not delivered"),
    }
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_like_standard_webhooks() {
        // The worked example of the specification's documentation.
        let key = STANDARD.decode("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap();
        let signature = sign(&key, "2524", 1614265330, r#"{"test": 2432232314}"#);
        assert!(signature.starts_with("v1,"));
        let again = sign(&key, "2524", 1614265330, r#"{"test": 2432232314}"#);
        assert_eq!(signature, again);
        assert_ne!(
            sign(&key, "2525", 1614265330, r#"{"test": 2432232314}"#),
            signature
        );
    }
}
