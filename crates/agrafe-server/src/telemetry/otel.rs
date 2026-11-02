//! The OpenTelemetry tracer provider, its OTLP exporter and the layer that
//! feeds it. See DESIGN.md, "Tracing".
//!
//! Only spans: metrics stay on `/metrics`, logs on stderr. A request or job
//! span's ids are the unit's trace context, by [`span_source`], so that the
//! `traceparent` a job row stores names a span the backend has.

use std::time::Duration;

use agrafe::config::{OtelConfig, OtelProtocol};
use agrafe::trace::{self, TraceContext};
use opentelemetry::trace::{
    SpanContext, SpanId, SpanKind, Status, TraceContextExt as _, TraceFlags, TraceId, TraceState,
    TracerProvider as _,
};
use opentelemetry::{Array, Context, KeyValue, StringValue, Value};
use opentelemetry_otlp::{
    Protocol, SpanExporter, WithExportConfig as _, WithHttpConfig as _, WithTonicConfig as _,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{
    BatchSpanProcessor, Sampler, SdkTracerProvider, Span, SpanData, SpanProcessor,
};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter;

use super::scrub::{LAST_ERROR, scrub};
use super::{Error, Output, exporter_filter};

/// How long the last batch has to leave once the process is stopping.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Sends what the batch still holds when dropped.
pub struct Guard(SdkTracerProvider);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Err(err) = self.0.shutdown_with_timeout(SHUTDOWN_TIMEOUT) {
            eprintln!("flushing the spans: {err}");
        }
    }
}

/// The provider and the layer, the units' ids plugged in. Inside the
/// runtime: tonic's channel spawns its worker there.
pub fn init(config: &OtelConfig, endpoint: &str) -> Result<(Guard, Output), Error> {
    let provider = provider(config, exporter(config, endpoint)?);
    let layer = layer(&provider);
    trace::set_span_source(span_source);
    Ok((Guard(provider), layer))
}

fn exporter(config: &OtelConfig, endpoint: &str) -> Result<SpanExporter, Error> {
    let otel_error = |err: opentelemetry_otlp::ExporterBuildError| Error::Otel(err.to_string());
    match config.protocol {
        OtelProtocol::Grpc => {
            use opentelemetry_otlp::tonic_types::metadata::MetadataMap;
            use opentelemetry_otlp::tonic_types::transport::ClientTlsConfig;

            let mut metadata = MetadataMap::new();
            for (name, value) in &config.headers {
                // Checked when the configuration loaded.
                let (Ok(name), Ok(value)) =
                    (name.parse::<http::HeaderName>(), value.expose().parse())
                else {
                    return Err(Error::Otel(format!("otel.headers: {name:?} is not valid")));
                };
                metadata.as_mut().insert(name, value);
            }
            let builder = SpanExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint)
                .with_metadata(metadata);
            let builder = if endpoint.starts_with("https:") {
                builder.with_tls_config(ClientTlsConfig::new().with_enabled_roots())
            } else {
                builder
            };
            builder.build().map_err(otel_error)
        }
        OtelProtocol::HttpProtobuf => SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(http_endpoint(endpoint))
            .with_headers(
                config
                    .headers
                    .iter()
                    .map(|(name, value)| (name.clone(), value.expose().to_string()))
                    .collect(),
            )
            .build()
            .map_err(otel_error),
    }
}

/// The traces URL: the signal's path after a bare host, as a collector's
/// address is usually given; a URL with a path is taken as it is.
fn http_endpoint(endpoint: &str) -> String {
    let bare = endpoint
        .split_once("://")
        .is_some_and(|(_, rest)| !rest.trim_end_matches('/').contains('/'));
    if bare {
        format!("{}/v1/traces", endpoint.trim_end_matches('/'))
    } else {
        endpoint.to_string()
    }
}

fn provider<E>(config: &OtelConfig, exporter: E) -> SdkTracerProvider
where
    E: opentelemetry_sdk::trace::SpanExporter + 'static,
{
    provider_with(config, BatchSpanProcessor::builder(exporter).build())
}

fn provider_with(
    config: &OtelConfig,
    processor: impl SpanProcessor + 'static,
) -> SdkTracerProvider {
    SdkTracerProvider::builder()
        .with_span_processor(Scrubbing(processor))
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            config.sample_ratio,
        ))))
        .with_resource(
            Resource::builder()
                .with_service_name(config.service_name.clone())
                .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
                .build(),
        )
        .build()
}

/// At `info`, which is where every span of the library is: lower levels
/// are for reading on a terminal, not for paying to store.
fn layer(provider: &SdkTracerProvider) -> Output {
    tracing_opentelemetry::layer()
        .with_tracer(provider.tracer("agrafe"))
        .with_location(false)
        .with_threads(false)
        .with_filter(exporter_filter(LevelFilter::INFO))
        .boxed()
}

/// Make `parent` the span's parent, or the span a root, and answer with
/// the ids and the sampling decision the provider gave it.
fn span_source(span: &tracing::Span, parent: Option<TraceContext>) -> Option<TraceContext> {
    let parent_context = match parent {
        Some(parent) => Context::new().with_remote_span_context(SpanContext::new(
            TraceId::from_hex(&parent.trace_id().to_string()).ok()?,
            SpanId::from_hex(&parent.span_id().to_string()).ok()?,
            if parent.sampled() {
                TraceFlags::SAMPLED
            } else {
                TraceFlags::NOT_SAMPLED
            },
            true,
            TraceState::default(),
        )),
        None => Context::new(),
    };
    span.set_parent(parent_context).ok()?;
    let context = span.context();
    let span = context.span();
    let ids = span.span_context();
    if !ids.is_valid() {
        return None;
    }
    TraceContext::parse(&format!(
        "00-{}-{}-{:02x}",
        ids.trace_id(),
        ids.span_id(),
        ids.trace_flags().to_u8()
    ))
}

/// Every string of a finished span, on its way to the batch. The tracing
/// layer has no filter on fields, so it happens here.
#[derive(Debug)]
struct Scrubbing<P>(P);

impl<P: SpanProcessor> SpanProcessor for Scrubbing<P> {
    fn on_start(&self, span: &mut Span, context: &Context) {
        self.0.on_start(span, context);
    }

    fn on_end(&self, span: SpanData) {
        self.0.on_end(scrubbed(span));
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource);
    }
}

fn scrubbed(mut span: SpanData) -> SpanData {
    named(&mut span);
    scrub_attributes(&mut span.attributes);
    if let Status::Error { description } = &mut span.status {
        *description = scrub(description).into_owned().into();
    }
    for event in &mut span.events.events {
        event.name = scrub(&event.name).into_owned().into();
        event
            .attributes
            .retain(|attribute| attribute.key.as_str() != LAST_ERROR);
        scrub_attributes(&mut event.attributes);
    }
    span
}

/// What a backend groups by, and the request's `path` left behind, which
/// holds a filename where `http.route` has a placeholder. The names are
/// the template and the kind: low cardinality, and no user data.
fn named(span: &mut SpanData) {
    fn value<'a>(span: &'a SpanData, key: &str) -> Option<&'a Value> {
        span.attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .map(|attribute| &attribute.value)
    }
    match span.name.as_ref() {
        "request" => {
            if let (Some(method), Some(route)) = (value(span, "method"), value(span, "http.route"))
            {
                span.name = format!("{method} {route}").into();
            }
            span.span_kind = SpanKind::Server;
            span.attributes
                .retain(|attribute| attribute.key.as_str() != "path");
        }
        "job" => {
            if let Some(kind) = value(span, "kind") {
                span.name = format!("job {kind}").into();
            }
            span.span_kind = SpanKind::Consumer;
        }
        _ => {}
    }
}

fn scrub_attributes(attributes: &mut [KeyValue]) {
    for attribute in attributes {
        match &mut attribute.value {
            Value::String(text) => *text = scrub_string(text),
            Value::Array(Array::String(texts)) => {
                for text in texts {
                    *text = scrub_string(text);
                }
            }
            _ => {}
        }
    }
}

fn scrub_string(text: &StringValue) -> StringValue {
    scrub(text.as_str()).into_owned().into()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;

    const TOKEN: &str = "v1.k1.2000000000.c2lnbmF0dXJl";
    const ENVELOPE: &str = "d3JhcHBlZC1kYXRhLWtleQ";

    /// What reaches the batch.
    #[derive(Clone, Debug, Default)]
    struct Captured(Arc<Mutex<Vec<SpanData>>>);

    impl SpanProcessor for Captured {
        fn on_start(&self, _: &mut Span, _: &Context) {}

        fn on_end(&self, span: SpanData) {
            self.0.lock().unwrap().push(span);
        }

        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }

        fn shutdown_with_timeout(&self, _: Duration) -> OTelSdkResult {
            Ok(())
        }
    }

    impl Captured {
        fn named(&self, name: &str) -> SpanData {
            let spans = self.0.lock().unwrap();
            spans
                .iter()
                .find(|span| span.name == name)
                .cloned()
                .unwrap_or_else(|| panic!("no span {name} in {spans:?}"))
        }
    }

    fn with_otel(ratio: f64, body: impl FnOnce()) -> Captured {
        let captured = Captured::default();
        let config = OtelConfig {
            sample_ratio: ratio,
            ..OtelConfig::default()
        };
        let provider = provider_with(&config, captured.clone());
        let subscriber = tracing_subscriber::registry().with(layer(&provider));
        tracing::subscriber::with_default(subscriber, body);
        provider.force_flush().unwrap();
        captured
    }

    #[test]
    fn no_string_of_a_span_keeps_a_secret() {
        let url = format!("/blobs/{TOKEN}/abc/original/secret.pdf?e={ENVELOPE}");
        let captured = with_otel(1.0, || {
            let span = tracing::info_span!(
                "request",
                method = "GET",
                path = %url,
                "http.route" = "/blobs/{token}/{id}/original/{filename}",
                other = %url,
            );
            span.in_scope(|| {
                tracing::info!(url = %url, "reading {url}");
                tracing::error!("reading {url} failed");
            });
        });
        let spans = format!("{:?}", captured.0.lock().unwrap());
        for secret in [TOKEN, ENVELOPE, "secret.pdf"] {
            assert!(!spans.contains(secret), "{secret} in {spans}");
        }
        let request = captured.named("GET /blobs/{token}/{id}/original/{filename}");
        assert_eq!(request.span_kind, SpanKind::Server);
        assert!(
            request
                .attributes
                .iter()
                .all(|attribute| attribute.key.as_str() != "path"),
            "{request:?}"
        );
        assert!(
            matches!(request.status, Status::Error { .. }),
            "{request:?}"
        );
    }

    #[test]
    fn the_http_crates_stay_capped() {
        let captured = with_otel(1.0, || {
            let span = tracing::debug_span!(target: "hyper", "presigned-span");
            span.in_scope(|| {});
            let parent = tracing::info_span!("parent");
            parent.in_scope(|| {
                tracing::debug!(target: "hyper", "presigned");
                tracing::trace!(target: "h2", "presigned");
                tracing::debug!(target: "reqwest", "presigned");
                tracing::debug!(target: "hyper_util", "presigned");
                tracing::info!("kept");
            });
        });
        let spans = format!("{:?}", captured.0.lock().unwrap());
        assert!(spans.contains("kept"), "{spans}");
        assert!(!spans.contains("presigned"), "{spans}");
    }

    /// A unit's context is its span's ids, and a unit started from the
    /// stored context is that span's child.
    #[test]
    fn a_units_context_is_its_span() {
        let mut stored = None;
        let captured = with_otel(1.0, || {
            let request =
                tracing::info_span!("request", method = "PUT", trace_id = tracing::field::Empty);
            let context = span_source(&request, None).unwrap();
            stored = Some(context.to_string());
            drop(request);

            let job = tracing::info_span!("job", kind = "scan");
            let parent = TraceContext::parse(stored.as_deref().unwrap());
            let child = span_source(&job, parent).unwrap();
            assert_eq!(child.trace_id().to_string(), context.trace_id().to_string());
            assert_ne!(child.span_id().to_string(), context.span_id().to_string());
            assert!(child.sampled());
        });
        let request = captured.named("request");
        let job = captured.named("job scan");
        assert_eq!(request.parent_span_id, SpanId::INVALID);
        assert_eq!(job.parent_span_id, request.span_context.span_id());
        assert_eq!(job.span_context.trace_id(), request.span_context.trace_id());
        assert_eq!(
            stored.unwrap(),
            format!(
                "00-{}-{}-01",
                request.span_context.trace_id(),
                request.span_context.span_id()
            )
        );
    }

    /// The sampler decides at the root, the decision goes on the stored
    /// context, and the job's run follows it: no half-kept trace.
    #[test]
    fn an_unsampled_trace_stays_unsampled_through_its_jobs() {
        let captured = with_otel(0.0, || {
            let request = tracing::info_span!("request");
            let context = span_source(&request, None).unwrap();
            assert!(!context.sampled(), "{context}");
            let job = tracing::info_span!("job", kind = "scan");
            let child = span_source(&job, Some(context)).unwrap();
            assert!(!child.sampled(), "{child}");
            assert_eq!(child.trace_id().to_string(), context.trace_id().to_string());
        });
        assert_eq!(captured.0.lock().unwrap().len(), 0);
    }

    /// A span the exporter's filter leaves out has no ids to lend: the
    /// caller falls back on generated ones.
    #[test]
    fn a_filtered_span_has_no_ids() {
        with_otel(1.0, || {
            let span = tracing::debug_span!("quiet");
            assert_eq!(span_source(&span, None), None);
        });
    }

    #[test]
    fn the_http_endpoint_takes_the_signals_path_after_a_bare_host() {
        assert_eq!(http_endpoint("http://c:4318"), "http://c:4318/v1/traces");
        assert_eq!(http_endpoint("http://c:4318/"), "http://c:4318/v1/traces");
        assert_eq!(
            http_endpoint("https://c/otlp/v1/traces"),
            "https://c/otlp/v1/traces"
        );
    }
}
