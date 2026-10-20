//! Object store construction from configuration.

use std::sync::Arc;

use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use object_store::azure::MicrosoftAzureBuilder;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::local::LocalFileSystem;

use crate::config::StorageConfig;

/// The object store every blob's objects live in, shared across the
/// process.
pub type Store = Arc<dyn ObjectStore>;

/// A store that cannot be built from its configuration.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The `local` backend's directory did not exist and could not be
    /// created.
    #[error("creating storage directory {path}: {source}")]
    CreateDir {
        /// The configured `storage.local.path`.
        path: String,
        /// Why creating it failed.
        source: std::io::Error,
    },
    /// The backend refused its settings: a malformed key, a missing bucket
    /// name or region. Nothing is contacted, so an unreachable store is not
    /// seen here.
    #[error("opening the object store: {0}")]
    Open(#[from] object_store::Error),
}

/// The store `config` describes, settings left out filled from the
/// backend's own environment variables. Creates the `local` backend's
/// directory if needed; connects to nothing.
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
        StorageConfig::Azure { azure } => {
            let mut builder = MicrosoftAzureBuilder::from_env()
                .with_account(&azure.account)
                .with_container_name(&azure.container);
            if let Some(key) = &azure.access_key {
                builder = builder.with_access_key(key.expose());
            }
            if let Some(endpoint) = &azure.endpoint {
                builder = builder.with_endpoint(endpoint.clone());
            }
            let store = builder.build()?;
            Ok(Arc::new(store))
        }
        StorageConfig::Gcs { gcs } => {
            let mut builder = GoogleCloudStorageBuilder::from_env().with_bucket_name(&gcs.bucket);
            if let Some(key) = &gcs.service_account_key {
                builder = builder.with_service_account_key(key.expose());
            }
            let store = builder.build()?;
            Ok(Arc::new(store))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(value: serde_json::Value) -> StorageConfig {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn s3_store_from_every_setting() {
        // Building connects to nothing; the settings are only checked.
        let store = build(&config(serde_json::json!({
            "backend": "s3",
            "s3": {
                "bucket": "agrafe-test",
                "region": "fr-par",
                "endpoint": "https://s3.fr-par.scw.cloud",
                "access_key_id": "key",
                "secret_access_key": "secret",
            },
        })))
        .unwrap();
        assert!(store.to_string().contains("agrafe-test"), "{store}");

        let store = build(&config(serde_json::json!({
            "backend": "s3",
            "s3": { "bucket": "agrafe-test" },
        })))
        .unwrap();
        assert!(store.to_string().contains("agrafe-test"), "{store}");
    }

    #[test]
    fn azure_store_from_every_setting() {
        let store = build(&config(serde_json::json!({
            "backend": "azure",
            "azure": {
                "account": "agrafeaccount",
                "container": "agrafe-test",
                "access_key": "c2VjcmV0",
                "endpoint": "http://127.0.0.1:10000/agrafeaccount",
            },
        })))
        .unwrap();
        assert!(store.to_string().contains("agrafe-test"), "{store}");

        let store = build(&config(serde_json::json!({
            "backend": "azure",
            "azure": { "account": "agrafeaccount", "container": "agrafe-test" },
        })))
        .unwrap();
        assert!(store.to_string().contains("agrafe-test"), "{store}");
    }

    #[test]
    fn gcs_store_checks_its_key() {
        // Without a key, the credentials come from the environment or the
        // metadata server, asked on the first request.
        let store = build(&config(serde_json::json!({
            "backend": "gcs",
            "gcs": { "bucket": "agrafe-test" },
        })))
        .unwrap();
        assert!(store.to_string().contains("agrafe-test"), "{store}");

        let err = build(&config(serde_json::json!({
            "backend": "gcs",
            "gcs": { "bucket": "agrafe-test", "service_account_key": "not json" },
        })))
        .err()
        .unwrap();
        assert!(matches!(err, Error::Open(_)), "{err}");
    }

    #[test]
    fn local_store_creates_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("objects/nested");
        build(&config(serde_json::json!({
            "backend": "local",
            "local": { "path": path },
        })))
        .unwrap();
        assert!(path.is_dir());

        // Under a file, there is no directory to make.
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        let err = build(&config(serde_json::json!({
            "backend": "local",
            "local": { "path": file.join("objects") },
        })))
        .err()
        .unwrap();
        assert!(matches!(err, Error::CreateDir { .. }), "{err}");
    }
}
