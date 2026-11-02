//! The Sentry client and its layers: `error` events as issues, the trail
//! of breadcrumbs before them, panics, and spans when asked. See
//! DESIGN.md, "Tracing".
//!
//! `sentry-tower` stays out, which is what captures a request's URL and
//! the client's address, and the layers leave span fields off events. What
//! is left goes through [`Scrubbed`], the transport itself: `before_send`
//! and `before_breadcrumb` would not see a transaction, for which the SDK
//! has no hook, and the transport sees everything that leaves.

use std::sync::Arc;
use std::time::Duration;

use agrafe::config::{ExportLevel, SentryConfig};
use agrafe::trace::TraceContext;
use sentry::integrations::tracing::{BreadcrumbLayer, ErrorLayer, SpanLayer};
use sentry::protocol::{Envelope, EnvelopeItem, Event, Transaction};
use sentry::transports::DefaultTransportFactory;
use sentry::{ClientInitGuard, ClientOptions, Transport, TransportFactory};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{FilterExt as _, LevelFilter, filter_fn};
use tracing_subscriber::registry::Registry;

use super::scrub::scrub;
use super::{Error, Output, exporter_filter};

/// The client, bound to the process's hub, and the layers to feed it.
pub fn init(config: &SentryConfig, dsn: &str) -> Result<(ClientInitGuard, Vec<Output>), Error> {
    let guard = sentry::init(options(
        config,
        dsn,
        Arc::new(Scrubbing(Arc::new(DefaultTransportFactory))),
    )?);
    Ok((guard, layers(config)))
}

fn options(
    config: &SentryConfig,
    dsn: &str,
    transport: Arc<dyn TransportFactory>,
) -> Result<ClientOptions, Error> {
    // The error names no part of the DSN, which holds a key.
    let dsn = dsn
        .parse()
        .map_err(|_| Error::Sentry("sentry.dsn is not a valid DSN".into()))?;
    let release = config
        .release
        .clone()
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    let mut options = ClientOptions::default()
        .sample_rate(config.sample_rate)
        .traces_sample_rate(config.traces_sample_rate);
    options.dsn = Some(dsn);
    options.release = Some(release.into());
    options.environment = config.environment.clone().map(Into::into);
    options.max_breadcrumbs = config.max_breadcrumbs;
    options.attach_stacktrace = config.attach_stacktrace;
    options.send_default_pii = config.send_default_pii;
    options.in_app_include = vec!["agrafe"];
    options.shutdown_timeout = Duration::from_secs(5);
    // Runs on the thread that captured, in the unit's task: the id that
    // finds the event's logs and spans.
    options.before_send = Some(Arc::new(|mut event| {
        if let Some(context) = TraceContext::current() {
            event
                .tags
                .insert("trace_id".into(), context.trace_id().to_string());
        }
        Some(event)
    }));
    options.transport = Some(transport);
    Ok(options)
}

/// One layer per job, each with its own filter. No span layer at a rate of
/// zero, whose spans would be built to be dropped.
fn layers(config: &SentryConfig) -> Vec<Output> {
    let event_level = level_filter(config.event_level);
    // An event is not its own breadcrumb as well.
    let below_events =
        filter_fn(move |metadata| LevelFilter::from_level(*metadata.level()) > event_level);
    let mut layers: Vec<Output> = vec![
        BreadcrumbLayer::new()
            .with_filter(exporter_filter(level_filter(config.breadcrumb_level)).and(below_events))
            .boxed(),
        ErrorLayer::new()
            .with_filter(exporter_filter(event_level))
            .boxed(),
    ];
    if config.traces_sample_rate > 0.0 {
        let spans: Box<dyn Layer<Registry> + Send + Sync> = SpanLayer::new()
            .with_filter(exporter_filter(LevelFilter::INFO))
            .boxed();
        layers.push(spans);
    }
    layers
}

fn level_filter(level: ExportLevel) -> LevelFilter {
    match level {
        ExportLevel::Error => LevelFilter::ERROR,
        ExportLevel::Warn => LevelFilter::WARN,
        ExportLevel::Info => LevelFilter::INFO,
        ExportLevel::Debug => LevelFilter::DEBUG,
        ExportLevel::Trace => LevelFilter::TRACE,
    }
}

/// Puts [`Scrubbed`] around the transport a factory makes.
struct Scrubbing(Arc<dyn TransportFactory>);

impl TransportFactory for Scrubbing {
    fn create_transport_with_options(
        &self,
        options: sentry::TransportOptions,
    ) -> Arc<dyn Transport> {
        Arc::new(Scrubbed(self.0.create_transport_with_options(options)))
    }
}

/// The last thing before the network: every item of every envelope, every
/// string in it, whichever field it landed in.
struct Scrubbed(Arc<dyn Transport>);

impl Transport for Scrubbed {
    fn send_envelope(&self, envelope: Envelope) {
        let mut scrubbed = Envelope::new().with_headers(envelope.headers().clone());
        for item in envelope.into_items().filter_map(scrub_item) {
            scrubbed.add_item(item);
        }
        if scrubbed.items().next().is_some() {
            self.0.send_envelope(scrubbed);
        }
    }

    fn flush(&self, timeout: Duration) -> bool {
        self.0.flush(timeout)
    }

    fn shutdown(&self, timeout: Duration) -> bool {
        self.0.shutdown(timeout)
    }
}

/// `None` for what cannot be scrubbed, or what agrafe has no business
/// sending: an item it does not know is not one it can vouch for.
fn scrub_item(item: EnvelopeItem) -> Option<EnvelopeItem> {
    match item {
        EnvelopeItem::Event(event) => Some(scrub_json::<Event<'static>>(&event)?.into()),
        EnvelopeItem::Transaction(transaction) => {
            Some(scrub_json::<Transaction<'static>>(&transaction)?.into())
        }
        item @ EnvelopeItem::ClientReport(_) => Some(item),
        _ => None,
    }
}

/// Through JSON, which reaches every string of a protocol type with many
/// without naming them. Off the hot path: events are rare, and spans come
/// here only at a non-zero `traces_sample_rate`.
fn scrub_json<T: Serialize + DeserializeOwned>(item: &T) -> Option<T> {
    let mut value = serde_json::to_value(item).ok()?;
    scrub_value(&mut value);
    serde_json::from_value(value).ok()
}

fn scrub_value(value: &mut Value) {
    match value {
        Value::String(text) => {
            if let std::borrow::Cow::Owned(scrubbed) = scrub(text) {
                *text = scrubbed;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(scrub_value),
        Value::Object(fields) => {
            // The request span's `path` holds a filename, and `http.route`
            // says the same without it; a storage span's goes too, which an
            // object key's blob id does not miss here. A job's `last_error`
            // may quote a user's file.
            fields.remove("path");
            fields.remove(super::scrub::LAST_ERROR);
            fields.values_mut().for_each(scrub_value);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use sentry::{Hub, Scope};
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;

    const TOKEN: &str = "v1.k1.2000000000.c2lnbmF0dXJl";
    const ENVELOPE: &str = "d3JhcHBlZC1kYXRhLWtleQ";
    const DSN: &str = "http://key@sentry.test/1";

    #[derive(Default)]
    struct Captured(Mutex<Vec<Envelope>>);

    impl Transport for Captured {
        fn send_envelope(&self, envelope: Envelope) {
            self.0.lock().unwrap().push(envelope);
        }
    }

    impl Captured {
        /// Everything sent, as it would be on the wire.
        fn wire(&self) -> String {
            let mut bytes = Vec::new();
            for envelope in self.0.lock().unwrap().iter() {
                envelope.to_writer(&mut bytes).unwrap();
            }
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    /// Run `body` with the layers of `config` feeding a client that sends
    /// through [`Scrubbed`] to the returned capture.
    fn with_sentry(config: &SentryConfig, body: impl FnOnce()) -> Arc<Captured> {
        let captured = Arc::new(Captured::default());
        let inner = captured.clone();
        let factory =
            move |_: &ClientOptions| -> Arc<dyn Transport> { Arc::new(Scrubbed(inner.clone())) };
        let client = sentry::Client::from_config(options(config, DSN, Arc::new(factory)).unwrap());
        let hub = Arc::new(Hub::new(Some(Arc::new(client)), Arc::new(Scope::default())));
        let subscriber = tracing_subscriber::registry().with(layers(config));
        Hub::run(hub, || {
            tracing::subscriber::with_default(subscriber, body);
            Hub::current().client().unwrap().flush(None);
        });
        captured
    }

    #[test]
    fn an_event_and_its_breadcrumbs_leave_nothing_behind() {
        let url = format!("/blobs/{TOKEN}/abc/original/secret.pdf?e={ENVELOPE}");
        let captured = with_sentry(&SentryConfig::default(), || {
            tracing::info!(url = %url, "reading {url}");
            tracing::error!(url = %url, path = %url, "reading {url} failed");
        });
        let wire = captured.wire();
        assert!(wire.contains("failed"), "the event was sent: {wire}");
        assert!(wire.contains("/blobs/<token>"), "{wire}");
        for secret in [TOKEN, ENVELOPE, "secret.pdf"] {
            assert!(!wire.contains(secret), "{secret} in {wire}");
        }
    }

    #[test]
    fn a_transaction_leaves_nothing_behind() {
        let config = SentryConfig {
            traces_sample_rate: 1.0,
            ..SentryConfig::default()
        };
        let path = format!("/blobs/{TOKEN}/abc/original/secret.pdf");
        let captured = with_sentry(&config, || {
            let span =
                tracing::info_span!("request", path = %path, query = %format!("/x?e={ENVELOPE}"));
            span.in_scope(|| tracing::info_span!("storage", op = "get").in_scope(|| {}));
        });
        let wire = captured.wire();
        assert!(
            wire.contains("\"transaction\""),
            "the span was sent: {wire}"
        );
        for secret in [TOKEN, ENVELOPE, "secret.pdf"] {
            assert!(!wire.contains(secret), "{secret} in {wire}");
        }
    }

    #[test]
    fn the_levels_say_what_becomes_what() {
        let captured = with_sentry(&SentryConfig::default(), || {
            tracing::debug!("too quiet");
            tracing::info!("a crumb");
            tracing::warn!("a louder crumb");
            tracing::error!("the event");
            // The HTTP crates' lines are below the cap at every setting.
            tracing::debug!(target: "hyper::proto", "GET https://s3.test/presigned");
        });
        let wire = captured.wire();
        assert!(wire.contains("the event"), "{wire}");
        assert!(wire.contains("a crumb"), "{wire}");
        assert!(wire.contains("a louder crumb"), "{wire}");
        assert!(!wire.contains("too quiet"), "{wire}");
        assert!(!wire.contains("presigned"), "{wire}");
        assert_eq!(
            wire.matches("the event").count(),
            1,
            "not a crumb too: {wire}"
        );
    }

    #[test]
    fn the_http_crates_stay_capped_at_any_level() {
        let config = SentryConfig {
            breadcrumb_level: ExportLevel::Trace,
            ..SentryConfig::default()
        };
        let captured = with_sentry(&config, || {
            tracing::trace!("a fine crumb");
            for target in super::super::HTTP_TARGETS {
                match target {
                    "hyper" => tracing::debug!(target: "hyper", "presigned"),
                    "hyper_util" => tracing::debug!(target: "hyper_util", "presigned"),
                    "h2" => tracing::trace!(target: "h2", "presigned"),
                    _ => tracing::debug!(target: "reqwest", "presigned"),
                }
            }
            tracing::error!("the event");
        });
        let wire = captured.wire();
        assert!(wire.contains("a fine crumb"), "{wire}");
        assert!(!wire.contains("presigned"), "{wire}");
    }

    #[test]
    fn an_event_says_its_trace() {
        let captured = Arc::new(Mutex::new(None));
        let seen = captured.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let context = TraceContext::root();
        let wire = with_sentry(&SentryConfig::default(), || {
            runtime.block_on(context.scope(async {
                tracing::error!("in a unit");
            }));
            *seen.lock().unwrap() = Some(());
        })
        .wire();
        assert!(captured.lock().unwrap().is_some());
        assert!(wire.contains(&context.trace_id().to_string()), "{wire}");
    }

    #[test]
    fn what_cannot_be_vouched_for_is_not_sent() {
        let inner = Arc::new(Captured::default());
        let transport = Scrubbed(inner.clone());
        let mut envelope = Envelope::new();
        envelope.add_item(sentry::protocol::Attachment {
            buffer: b"bytes".to_vec(),
            filename: "secret.pdf".into(),
            content_type: None,
            ty: None,
        });
        transport.send_envelope(envelope);
        assert_eq!(inner.0.lock().unwrap().len(), 0);
    }
}
