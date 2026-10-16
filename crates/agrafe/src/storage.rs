//! Object store construction from configuration.

use std::sync::Arc;

use anyhow::{Context, Result};
use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;

use crate::config::StorageConfig;

pub type Store = Arc<dyn ObjectStore>;

pub fn build(config: &StorageConfig) -> Result<Store> {
    match config {
        StorageConfig::Local { local } => {
            std::fs::create_dir_all(&local.path)
                .with_context(|| format!("creating storage directory {}", local.path))?;
            let store = LocalFileSystem::new_with_prefix(&local.path)
                .with_context(|| format!("opening local storage at {}", local.path))?;
            Ok(Arc::new(store))
        }
        StorageConfig::S3 { s3 } => {
            let mut builder = AmazonS3Builder::from_env().with_bucket_name(&s3.bucket);
            if let Some(region) = &s3.region {
                builder = builder.with_region(region);
            }
            if let Some(endpoint) = &s3.endpoint {
                builder = builder.with_endpoint(endpoint);
            }
            if let Some(key) = &s3.access_key_id {
                builder = builder.with_access_key_id(key);
            }
            if let Some(secret) = &s3.secret_access_key {
                builder = builder.with_secret_access_key(secret.expose());
            }
            let store = builder.build().context("building S3 client")?;
            Ok(Arc::new(store))
        }
    }
}
