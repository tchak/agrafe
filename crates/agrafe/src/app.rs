//! The handle everything hangs off: configuration, pool, store, keys,
//! signer, clamd client and metrics behind one `Arc`. The private API is a
//! set of methods on it, spread over the modules that implement them
//! (`blob`, `blob::create`, `blob::urls`, `upload`, `jobs`).

use std::sync::Arc;

use sqlx::PgPool;

use crate::clamav::Clamd;
use crate::config::{Config, ConfigError};
use crate::encryption::Keks;
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
    metrics: Metrics,
}

impl Agrafe {
    pub fn new(config: Config, pool: PgPool) -> Result<Self, InitError> {
        let store = storage::build(&config.storage)?;
        let signer = Signer::from_config(&config.signing)?;
        let keks = Keks::from_config(&config.encryption)?;
        let clamd = Clamd::from_config(&config.clamav);
        Ok(Self(Arc::new(Inner {
            config,
            pool,
            store,
            signer,
            keks,
            clamd,
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

    pub fn keks(&self) -> &Keks {
        &self.0.keks
    }

    pub fn clamd(&self) -> &Clamd {
        &self.0.clamd
    }

    pub fn metrics(&self) -> &Metrics {
        &self.0.metrics
    }
}
