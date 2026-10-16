//! Storing and reading back a blob's thumbnail and preview.
//!
//! They are small and served whole, so each is one object, encrypted with
//! the blob's own data key: the streaming AEAD derives a fresh subkey per
//! stream from a random salt, so one key safely covers the original and
//! both representations. The object key is the associated data, as for
//! the original, and the stream header travels in the object itself.

use agrafe_crypto::streaming;
use object_store::ObjectStoreExt;
use object_store::path::Path as ObjectPath;
use uuid::Uuid;

use super::read::{ReadError, Stored};
use super::{Blob, Representation};
use crate::app::AppState;

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
}

/// Encrypt `plaintext` and write it as `blob`'s `kind`, replacing any
/// previous version.
pub async fn store(
    state: &AppState,
    blob: &Blob,
    kind: Representation,
    plaintext: &[u8],
) -> Result<(), Error> {
    let stored = Stored::from_blob(blob)?;
    let key = state
        .keks()
        .unwrap(blob.id, &stored.kek_id, &stored.wrapped_key)
        .map_err(ReadError::from)?;
    let object_key = kind.object_key(blob.id);
    let sealed = streaming::encrypt(&key, object_key.as_bytes(), stored.layout, plaintext)
        .map_err(|source| Error::Seal {
            kind,
            blob_id: blob.id,
            source,
        })?;
    state
        .store()
        .put(&ObjectPath::from(object_key), sealed.into())
        .await?;
    Ok(())
}

/// Read and decrypt `blob`'s `kind`, or `None` when the object is not there.
pub async fn load(
    state: &AppState,
    blob: &Blob,
    kind: Representation,
) -> Result<Option<Vec<u8>>, Error> {
    let stored = Stored::from_blob(blob)?;
    let object_key = kind.object_key(blob.id);
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
        .unwrap(blob.id, &stored.kek_id, &stored.wrapped_key)
        .map_err(ReadError::from)?;
    let plain = streaming::decrypt(&key, object_key.as_bytes(), stored.layout, &sealed).map_err(
        |source| Error::Corrupt {
            kind,
            blob_id: blob.id,
            source,
        },
    )?;
    Ok(Some(plain))
}
