//! Storing and reading back a blob's thumbnail and preview.
//!
//! They are small and served whole, so each is one object, encrypted with
//! the blob's own data key: the streaming AEAD derives a fresh subkey per
//! stream from a random salt, so one key safely covers the original and
//! both representations. The object key is the associated data, as for
//! the original, and the stream header travels in the object itself.

use agrafe_crypto::streaming;
use jiff::Timestamp;
use object_store::ObjectStoreExt;
use object_store::path::Path as ObjectPath;
use uuid::Uuid;

use super::read::{ReadError, Stored};
use super::{BlobHead, BlobState, Representation, RepresentationState, repo};
use crate::app::Agrafe;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Read(#[from] ReadError),
    #[error("sealing the {kind} of blob {blob_id}: {source}")]
    Seal {
        kind: Representation,
        blob_id: Uuid,
        source: agrafe_crypto::Error,
    },
    #[error("the {kind} of blob {blob_id} is corrupt: {source}")]
    Corrupt {
        kind: Representation,
        blob_id: Uuid,
        source: agrafe_crypto::Error,
    },
    #[error("writing the object: {0}")]
    Store(#[from] object_store::Error),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

/// What a request for a representation resolves to.
#[derive(Debug)]
pub enum Resolved {
    /// The rendered WebP.
    Bytes(Vec<u8>),
    /// Not there, in this state: the caller shows an icon or a `404`.
    Placeholder(RepresentationState),
    /// The blob itself is not readable (yet, or ever).
    NotReady(BlobState),
}

impl Agrafe {
    /// What to serve for `kind` of `blob`, queueing a render when what is
    /// there is missing or was built with another variant version.
    pub async fn representation(
        &self,
        blob: &BlobHead,
        kind: Representation,
    ) -> Result<Resolved, Error> {
        resolve(self, blob, kind).await
    }
}

async fn resolve(state: &Agrafe, blob: &BlobHead, kind: Representation) -> Result<Resolved, Error> {
    let now = Timestamp::now();
    if !blob.is_readable(now) {
        return Ok(Resolved::NotReady(blob.state_at(now)));
    }
    let version = state.config().media.variant_version;
    let rep_state = blob.representation_state(kind);
    if !matches!(
        rep_state,
        RepresentationState::Pending | RepresentationState::Disabled
    ) && blob.representation_version(kind) != Some(version)
    {
        tracing::info!(blob = %blob.id, kind = %kind, "built with another variant version, regenerating");
        repo::request_representation(state.pool(), blob.id, kind).await?;
        return Ok(Resolved::Placeholder(RepresentationState::Pending));
    }
    match rep_state {
        RepresentationState::Ready => match load(state, &Stored::from_head(blob)?, kind).await? {
            Some(bytes) => Ok(Resolved::Bytes(bytes)),
            None => {
                tracing::warn!(blob = %blob.id, kind = %kind, "object is missing, regenerating");
                repo::request_representation(state.pool(), blob.id, kind).await?;
                Ok(Resolved::Placeholder(RepresentationState::Pending))
            }
        },
        other => Ok(Resolved::Placeholder(other)),
    }
}

/// Encrypt `plaintext` and write it as the `kind` of the blob `stored`
/// describes, replacing any previous version.
pub async fn store(
    state: &Agrafe,
    stored: &Stored,
    kind: Representation,
    plaintext: &[u8],
) -> Result<(), Error> {
    let blob_id = stored.blob_id;
    let key = state
        .keks()
        .unwrap(blob_id, &stored.kek_id, &stored.wrapped_key)
        .map_err(ReadError::from)?;
    let object_key = kind.object_key(blob_id);
    let sealed = streaming::encrypt(&key, object_key.as_bytes(), stored.layout, plaintext)
        .map_err(|source| Error::Seal {
            kind,
            blob_id,
            source,
        })?;
    state
        .store()
        .put(&ObjectPath::from(object_key), sealed.into())
        .await?;
    Ok(())
}

/// Read and decrypt the `kind` of the blob `stored` describes, or `None`
/// when the object is not there.
pub async fn load(
    state: &Agrafe,
    stored: &Stored,
    kind: Representation,
) -> Result<Option<Vec<u8>>, Error> {
    let blob_id = stored.blob_id;
    let object_key = kind.object_key(blob_id);
    let sealed = match state
        .store()
        .get(&ObjectPath::from(object_key.as_str()))
        .await
    {
        Ok(result) => result.bytes().await?,
        Err(object_store::Error::NotFound { .. }) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let key = state
        .keks()
        .unwrap(blob_id, &stored.kek_id, &stored.wrapped_key)
        .map_err(ReadError::from)?;
    let plain = streaming::decrypt(&key, object_key.as_bytes(), stored.layout, &sealed).map_err(
        |source| Error::Corrupt {
            kind,
            blob_id,
            source,
        },
    )?;
    Ok(Some(plain))
}
