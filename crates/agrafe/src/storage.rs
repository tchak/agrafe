//! Object store construction from configuration.

use std::sync::Arc;

use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;

use crate::config::StorageConfig;

pub type Store = Arc<dyn ObjectStore>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("creating storage directory {path}: {source}")]
    CreateDir {
        path: String,
        source: std::io::Error,
    },
    #[error("opening the object store: {0}")]
    Open(#[from] object_store::Error),
}

pub fn build(config: &StorageConfig) -> Result<Store, Error> {
    match config {
        StorageConfig::Local { local } => {
            std::fs::create_dir_all(&local.path).map_err(|source| Error::CreateDir {
                path: local.path.clone(),
                source,
            })?;
            let store = LocalFileSystem::new_with_prefix(&local.path)?;
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
            let store = builder.build()?;
            Ok(Arc::new(store))
        }
    }
}
