//! The rows the read routes served, kept for a while. See [DESIGN.md,
//! "Original"].
//!
//! Only a settled row is kept: `ready` or terminal, with no representation
//! pending. Such a row changes only by going terminal, which the cache
//! shows late by at most its TTL, the objects being gone only once the
//! purge ran anyway. A row in flux, an upload or a render under way, is
//! never kept, so polling it, and the `wait` of a representation request,
//! see every change as it lands. The read path's own writes, setting a
//! representation back to `pending`, drop the row from this process's
//! cache at once.
//!
//! [DESIGN.md, "Original"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#original

use std::time::Duration;

use uuid::Uuid;

use super::{BlobHead, BlobState, RepresentationState};
use crate::config::BlobsConfig;

/// A `None` is a row that is not there, kept like the others so a URL
/// with a forged id does not cost a query a time.
type Entry = Option<BlobHead>;

pub struct HeadCache {
    /// `None` when `blobs.read_cache_ttl` is zero.
    inner: Option<moka::sync::Cache<Uuid, Entry>>,
}

impl HeadCache {
    pub fn from_config(config: &BlobsConfig) -> Self {
        let inner = (config.read_cache_ttl > Duration::ZERO).then(|| {
            moka::sync::Cache::builder()
                .max_capacity(config.read_cache_entries)
                .time_to_live(config.read_cache_ttl)
                .build()
        });
        Self { inner }
    }

    pub fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn get(&self, id: Uuid) -> Option<Entry> {
        self.inner.as_ref()?.get(&id)
    }

    /// Keep what a lookup found, when it is settled.
    pub fn insert(&self, id: Uuid, entry: &Entry) {
        if let Some(cache) = &self.inner
            && entry.as_ref().is_none_or(settled)
        {
            cache.insert(id, entry.clone());
        }
    }

    pub fn invalidate(&self, id: Uuid) {
        if let Some(cache) = &self.inner {
            cache.invalidate(&id);
        }
    }
}

/// Whether nothing is under way on the row: it is `ready` or terminal and
/// no representation is pending.
fn settled(head: &BlobHead) -> bool {
    let state = matches!(
        head.state,
        BlobState::Ready | BlobState::Infected | BlobState::Expired | BlobState::Deleted
    );
    state
        && head.thumbnail_state != RepresentationState::Pending
        && head.preview_state != RepresentationState::Pending
}
