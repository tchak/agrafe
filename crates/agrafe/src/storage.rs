//! Object store construction from configuration.

use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use object_store::aws::AmazonS3Builder;
use object_store::azure::MicrosoftAzureBuilder;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, RenameOptions, Result, UploadPart,
};
use tracing::{Instrument as _, Span};

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

/// A store whose operations each run in a `storage` span, `op` and `path`
/// as fields: what the request or the job waited on, and for how long.
/// `Agrafe::with_store` wraps every store in it. A read's span ends with
/// the response head, before the body streams; a listing, a stream that
/// outlives any call, and the deletes of `delete_stream` have none.
#[derive(Debug)]
pub struct Traced(Store);

impl Traced {
    /// `store`, traced.
    pub fn new(store: Store) -> Self {
        Self(store)
    }
}

fn span(op: &'static str, path: &Path) -> Span {
    tracing::info_span!("storage", op, path = %path)
}

impl std::fmt::Display for Traced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

// Every method forwarded, the provided ones too: a backend's own
// `get_ranges` or `rename_opts` is better than the trait's default.
#[async_trait]
#[deny(clippy::missing_trait_methods)]
impl ObjectStore for Traced {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        self.0
            .put_opts(location, payload, opts)
            .instrument(span("put", location))
            .await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        let upload = self
            .0
            .put_multipart_opts(location, opts)
            .instrument(span("put_multipart", location))
            .await?;
        Ok(Box::new(TracedUpload {
            inner: upload,
            path: location.clone(),
        }))
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        let op = if options.head { "head" } else { "get" };
        self.0
            .get_opts(location, options)
            .instrument(span(op, location))
            .await
    }

    async fn get_ranges(&self, location: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        self.0
            .get_ranges(location, ranges)
            .instrument(span("get_ranges", location))
            .await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        self.0.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.0.list(prefix)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, Result<ObjectMeta>> {
        self.0.list_with_offset(prefix, offset)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        let root = Path::default();
        self.0
            .list_with_delimiter(prefix)
            .instrument(span("list_with_delimiter", prefix.unwrap_or(&root)))
            .await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        self.0
            .copy_opts(from, to, options)
            .instrument(span("copy", to))
            .await
    }

    async fn rename_opts(&self, from: &Path, to: &Path, options: RenameOptions) -> Result<()> {
        self.0
            .rename_opts(from, to, options)
            .instrument(span("rename", to))
            .await
    }
}

/// A multipart upload whose parts, completion and abort are traced, under
/// the span current where each is called.
#[derive(Debug)]
struct TracedUpload {
    inner: Box<dyn MultipartUpload>,
    path: Path,
}

#[async_trait]
#[deny(clippy::missing_trait_methods)]
impl MultipartUpload for TracedUpload {
    fn put_part(&mut self, data: PutPayload) -> UploadPart {
        Box::pin(
            self.inner
                .put_part(data)
                .instrument(span("put_part", &self.path)),
        )
    }

    async fn complete(&mut self) -> Result<PutResult> {
        let span = span("complete", &self.path);
        self.inner.complete().instrument(span).await
    }

    async fn abort(&mut self) -> Result<()> {
        let span = span("abort", &self.path);
        self.inner.abort().instrument(span).await
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
