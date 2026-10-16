//! Shared application state.

use std::sync::Arc;

use anyhow::Result;
use sqlx::PgPool;

use crate::config::Config;
use crate::encryption::Keks;
use crate::signing::Signer;
use crate::storage::{self, Store};

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

struct Inner {
    config: Config,
    pool: PgPool,
    store: Store,
    signer: Signer,
    keks: Keks,
}

impl AppState {
    pub fn new(config: Config, pool: PgPool) -> Result<Self> {
        let store = storage::build(&config.storage)?;
        let signer = Signer::from_config(&config.signing)?;
        let keks = Keks::from_config(&config.encryption)?;
        Ok(Self(Arc::new(Inner {
            config,
            pool,
            store,
            signer,
            keks,
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
}
