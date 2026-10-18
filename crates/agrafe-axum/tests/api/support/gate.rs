//! A store that can be held at one operation on one object, for the tests
//! that change the row in the moment between an ownership check and the
//! write that relies on it.

use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use agrafe::storage::Store;
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result, UploadPart,
};
use tokio::sync::Notify;

/// The operations a hold can be set on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum At {
    /// Reading the object, or its metadata.
    Get,
    /// Completing a multipart upload to it: every byte is written and the
    /// object is about to become visible.
    Complete,
}

/// One operation caught by the store: it signals `reached` and goes
/// through once `release` is called.
#[derive(Clone, Default)]
pub struct Hold {
    reached: Arc<Notify>,
    release: Arc<Notify>,
}

impl Hold {
    pub async fn reached(&self) {
        self.reached.notified().await;
    }

    pub fn release(&self) {
        self.release.notify_one();
    }
}

/// Where the next hold is set; shared by the test and its store.
#[derive(Clone, Default)]
pub struct Gate(Arc<Mutex<Option<(At, Path, Hold)>>>);

impl fmt::Debug for Gate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Gate")
    }
}

impl Gate {
    /// Hold the next `at` on `key`, once.
    pub fn hold(&self, at: At, key: &str) -> Hold {
        let hold = Hold::default();
        *self.0.lock().unwrap() = Some((at, Path::from(key), hold.clone()));
        hold
    }

    async fn pass(&self, at: At, location: &Path) {
        let hold = {
            let mut slot = self.0.lock().unwrap();
            match &*slot {
                Some((a, p, _)) if *a == at && p == location => slot.take().map(|(_, _, h)| h),
                _ => None,
            }
        };
        if let Some(hold) = hold {
            hold.reached.notify_one();
            hold.release.notified().await;
        }
    }
}

#[derive(Debug)]
pub struct GatedStore {
    inner: Store,
    gate: Gate,
}

impl GatedStore {
    pub fn new(inner: Store, gate: Gate) -> Self {
        Self { inner, gate }
    }
}

impl fmt::Display for GatedStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Gated({})", self.inner)
    }
}

#[async_trait]
impl ObjectStore for GatedStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        let inner = self.inner.put_multipart_opts(location, opts).await?;
        Ok(Box::new(GatedUpload {
            inner,
            gate: self.gate.clone(),
            location: location.clone(),
        }))
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        self.gate.pass(At::Get, location).await;
        self.inner.get_opts(location, options).await
    }

    async fn get_ranges(&self, location: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        self.gate.pass(At::Get, location).await;
        self.inner.get_ranges(location, ranges).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[derive(Debug)]
struct GatedUpload {
    inner: Box<dyn MultipartUpload>,
    gate: Gate,
    location: Path,
}

#[async_trait]
impl MultipartUpload for GatedUpload {
    fn put_part(&mut self, data: PutPayload) -> UploadPart {
        self.inner.put_part(data)
    }

    async fn complete(&mut self) -> Result<PutResult> {
        self.gate.pass(At::Complete, &self.location).await;
        self.inner.complete().await
    }

    async fn abort(&mut self) -> Result<()> {
        self.inner.abort().await
    }
}
