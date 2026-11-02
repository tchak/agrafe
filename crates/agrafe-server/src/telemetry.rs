//! What the process's spans and events go to: the log lines on stderr, and
//! the Sentry and OpenTelemetry exporters when configured. See DESIGN.md,
//! "Tracing". The exporters live here, in the binary, so that the library
//! and the routers depend on neither and a host brings its own.
//!
//! Each output has its own filter. `RUST_LOG` is the logs': raising it to
//! read something on a terminal does not send more of it anywhere, and an
//! operator naming `hyper=debug` for the logs does not lift the exporters'
//! cap on the HTTP crates, which log presigned URLs.

#[cfg(feature = "otel")]
mod otel;
#[cfg(any(feature = "otel", feature = "sentry"))]
mod scrub;
#[cfg(feature = "sentry")]
mod sentry;

use agrafe::config::Config;
use tracing_subscriber::filter::LevelFilter;
#[cfg(any(feature = "otel", feature = "sentry"))]
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry::Registry;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer};

/// An output for the subscriber, filtered on its own.
type Output = Box<dyn Layer<Registry> + Send + Sync>;

/// Why the exporters could not start. The process refuses to run: one that
/// was told to report and silently does not is found out at the outage.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The configuration asks for what this build left out.
    #[cfg(not(all(feature = "sentry", feature = "otel")))]
    #[error("[{0}] is configured but this build has no {0} support: build with the `{0}` feature")]
    NotBuilt(&'static str),
    /// The Sentry client could not be set up.
    #[cfg(feature = "sentry")]
    #[error("starting the Sentry client: {0}")]
    Sentry(String),
    /// The OTLP exporter could not be set up.
    #[cfg(feature = "otel")]
    #[error("starting the OTLP exporter: {0}")]
    Otel(String),
}

/// Holds the exporters for the life of the process; dropping it sends what
/// they still hold.
#[derive(Default)]
pub struct Telemetry {
    #[cfg(feature = "sentry")]
    _sentry: Option<::sentry::ClientInitGuard>,
    #[cfg(feature = "otel")]
    _otel: Option<otel::Guard>,
}

/// Install the process's subscriber. `log_filter` is the value of
/// `RUST_LOG`, for the log lines alone.
pub fn init(config: &Config, log_filter: &str) -> Result<Telemetry, Error> {
    #[cfg_attr(not(any(feature = "sentry", feature = "otel")), expect(unused_mut))]
    let mut telemetry = Telemetry::default();
    // Logs on stderr: stdout is the audit's report, to keep or pipe apart.
    // Coloured on a terminal only, not in a log collector's input.
    #[cfg_attr(not(any(feature = "sentry", feature = "otel")), expect(unused_mut))]
    let mut outputs: Vec<Output> = vec![
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
            .with_filter(cap_http_targets(env_filter(log_filter), log_filter))
            .boxed(),
    ];

    #[cfg(feature = "sentry")]
    if let Some(dsn) = config.sentry.dsn() {
        let (guard, layers) = sentry::init(&config.sentry, dsn)?;
        telemetry._sentry = Some(guard);
        outputs.extend(layers);
    }
    #[cfg(not(feature = "sentry"))]
    if config.sentry.dsn().is_some() {
        return Err(Error::NotBuilt("sentry"));
    }

    #[cfg(feature = "otel")]
    if let Some(endpoint) = config.otel.endpoint() {
        let (guard, layer) = otel::init(&config.otel, endpoint)?;
        telemetry._otel = Some(guard);
        outputs.push(layer);
    }
    #[cfg(not(feature = "otel"))]
    if config.otel.endpoint().is_some() {
        return Err(Error::NotBuilt("otel"));
    }

    tracing_subscriber::registry().with(outputs).init();
    Ok(telemetry)
}

/// The crates under the fetch client, which log request targets at debug
/// and trace. A fetched URL is a presigned secret.
const HTTP_TARGETS: [&str; 4] = ["hyper", "hyper_util", "h2", "reqwest"];

/// The crates that do the exporting: exported, their own events would feed
/// the exporter they describe.
#[cfg(any(feature = "otel", feature = "sentry"))]
const EXPORTER_TARGETS: [&str; 7] = [
    "opentelemetry",
    "opentelemetry_sdk",
    "opentelemetry_otlp",
    "opentelemetry_http",
    "tonic",
    "tower",
    "sentry",
];

/// An exporter's filter: `level` and up, the HTTP crates at `info` at
/// most, unconditionally, and the exporting crates not at all.
#[cfg(any(feature = "otel", feature = "sentry"))]
fn exporter_filter(level: LevelFilter) -> Targets {
    let capped = HTTP_TARGETS
        .into_iter()
        .fold(Targets::new().with_default(level), |targets, target| {
            targets.with_target(target, level.min(LevelFilter::INFO))
        });
    EXPORTER_TARGETS
        .into_iter()
        .fold(capped, |targets, target| {
            targets.with_target(target, LevelFilter::OFF)
        })
}

/// Hold the HTTP stack at `info` whatever the global level says, unless
/// the filter names one of those crates itself: then the operator asked
/// and gets it. The check is on the filter string, since a directive added
/// on a target replaces the one the filter already had on it; a directive
/// on a module below, `hyper::proto=trace`, is more specific and wins over
/// the cap on its own.
fn cap_http_targets(filter: EnvFilter, env: &str) -> EnvFilter {
    HTTP_TARGETS
        .into_iter()
        .filter(|target| !names_target(env, target))
        .fold(filter, |filter, target| {
            filter.add_directive(format!("{target}=info").parse().expect("a valid directive"))
        })
}

/// The filter `env`, the value of `RUST_LOG`, spells, `info` when it is
/// empty or names nothing valid. A directive it cannot parse is reported on stderr and
/// left out, the others kept: one typo must not silently turn the debug
/// lines an operator asked for back into `info`.
fn env_filter(env: &str) -> EnvFilter {
    EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .parse_lossy(env)
}

/// Whether a filter string has a directive on exactly `target`:
/// `hyper=debug`, `hyper[span]=trace`, or a bare `hyper`.
fn names_target(env: &str, target: &str) -> bool {
    env.split(',')
        .map(str::trim)
        .any(|directive| directive.split(['=', '[']).next() == Some(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_http_stack_is_capped_unless_named() {
        assert!(!names_target("debug", "hyper"));
        assert!(!names_target("hyper_util=trace", "hyper"), "another crate");
        assert!(
            !names_target("hyper::proto=trace", "hyper"),
            "a module below"
        );
        assert!(names_target("info,hyper=debug", "hyper"));
        assert!(names_target("hyper[span]=trace", "hyper"));
        assert!(names_target(" hyper ", "hyper"));

        let capped = cap_http_targets(EnvFilter::new("debug"), "debug").to_string();
        assert!(capped.contains("hyper=info"), "{capped}");
        assert!(capped.contains("reqwest=info"), "{capped}");
        let asked = cap_http_targets(EnvFilter::new("hyper=trace"), "hyper=trace").to_string();
        assert!(asked.contains("hyper=trace"), "{asked}");
        assert!(!asked.contains("hyper=info"), "{asked}");
        assert!(asked.contains("hyper_util=info"), "{asked}");
    }

    #[test]
    fn a_bad_directive_leaves_the_others() {
        let filter = env_filter("debug,hyper=trce").to_string();
        assert!(filter.contains("debug"), "{filter}");
        assert!(!filter.contains("trce"), "{filter}");
        let unset = env_filter("").to_string();
        assert!(unset.contains("info"), "{unset}");
        let targeted = env_filter("agrafe=debug").to_string();
        assert!(targeted.contains("agrafe=debug"), "{targeted}");
    }
}
