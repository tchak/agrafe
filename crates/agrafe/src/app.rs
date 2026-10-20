//! The handle everything hangs off: configuration, pool, store, keys,
//! signer, clamd client and metrics behind one `Arc`. The private API is a
//! set of methods on it, spread over the modules that implement them
//! (`blob`, `blob::create`, `blob::urls`, `blob::representation`,
//! `upload`, `source`, `attachment`, `jobs`).

use std::sync::Arc;

use sqlx::PgPool;

use crate::blob::cache::HeadCache;
use crate::clamav::Clamd;
use crate::config::{Config, ConfigError};
use crate::encryption::Keks;
use crate::jobs::{fetch, notify};
use crate::metrics::Metrics;
use crate::signing::Signer;
use crate::storage::{self, Store};

/// Why the state could not be built from a configuration that loaded.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error(transparent)]
    Storage(#[from] storage::Error),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("building the fetch client: {0}")]
    Fetch(#[from] reqwest::Error),
    #[error("building the map layer client: {0}")]
    Maps(reqwest::Error),
    #[error("building the webhook client: {0}")]
    Webhook(reqwest::Error),
}

/// A cheap-to-clone handle on one agrafe instance.
#[derive(Clone)]
pub struct Agrafe(Arc<Inner>);

struct Inner {
    config: Config,
    pool: PgPool,
    store: Store,
    signer: Signer,
    keks: Keks,
    clamd: Clamd,
    fetcher: fetch::Client,
    maps: agrafe_render::geo::Client,
    webhook: Option<notify::Client>,
    heads: HeadCache,
    metrics: Metrics,
}

impl Agrafe {
    pub fn new(config: Config, pool: PgPool) -> Result<Self, InitError> {
        let store = storage::build(&config.storage)?;
        Self::with_store(config, pool, store)
    }

    /// `new` over a store built elsewhere; `config.storage` goes unused.
    pub fn with_store(config: Config, pool: PgPool, store: Store) -> Result<Self, InitError> {
        let signer = Signer::from_config(&config.signing)?;
        let keks = Keks::from_config(&config.encryption)?;
        let clamd = Clamd::from_config(&config.clamav);
        let fetcher = fetch::Client::from_config(&config.fetch)?;
        let maps = agrafe_render::geo::Client::from_config(&config.media.render.geojson)
            .map_err(InitError::Maps)?;
        let webhook = match &config.webhook.url {
            Some(url) => Some(
                notify::Client::new(url, config.webhook.key()?, config.webhook.timeout)
                    .map_err(InitError::Webhook)?,
            ),
            None => None,
        };
        let heads = HeadCache::from_config(&config.blobs);
        Ok(Self(Arc::new(Inner {
            config,
            pool,
            store,
            signer,
            keks,
            clamd,
            fetcher,
            maps,
            webhook,
            heads,
            metrics: Metrics::new(),
        })))
    }

    pub fn config(&self) -> &Config {
        &self.0.config
    }

    pub fn pool(&self) -> &PgPool {
        &self.0.pool
    }

    pub fn store(&self) -> &Store {
        &self.0.store
    }

    pub fn signer(&self) -> &Signer {
        &self.0.signer
    }

    // Public for the integration tests, not part of the API.
    #[doc(hidden)]
    pub fn keks(&self) -> &Keks {
        &self.0.keks
    }

    // Public for the integration tests, not part of the API.
    #[doc(hidden)]
    pub fn clamd(&self) -> &Clamd {
        &self.0.clamd
    }

    /// The client `url` blobs are fetched through.
    pub(crate) fn fetcher(&self) -> &fetch::Client {
        &self.0.fetcher
    }

    /// The client the background layers of a map are fetched through.
    pub(crate) fn maps(&self) -> &agrafe_render::geo::Client {
        &self.0.maps
    }

    /// The client deliveries go through; `None` without a `[webhook]` URL.
    // Public for the integration tests, not part of the API.
    #[doc(hidden)]
    pub fn webhook(&self) -> Option<&notify::Client> {
        self.0.webhook.as_ref()
    }

    /// The rows the read routes served lately.
    pub(crate) fn heads(&self) -> &HeadCache {
        &self.0.heads
    }

    pub fn metrics(&self) -> &Metrics {
        &self.0.metrics
    }
}
