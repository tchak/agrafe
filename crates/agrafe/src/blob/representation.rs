//! Storing and reading back a blob's thumbnail and preview.
//!
//! They are small and served whole, so each is one object, encrypted with
//! the blob's own data key: the streaming AEAD derives a fresh subkey per
//! stream from a random salt, so one key safely covers the original and
//! both representations. The object key is the associated data, as for
//! the original, and the stream header travels in the object itself.

use agrafe_crypto::streaming;
use bytes::{Bytes, BytesMut};
use jiff::Timestamp;
use object_store::ObjectStoreExt;
use object_store::path::Path as ObjectPath;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::read::{ReadError, Stored};
use super::{BlobHead, BlobState, Rendered, Representation, RepresentationState, repo};
use crate::app::Agrafe;

/// Why a representation could not be resolved, stored or loaded.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The row lacks what reading needs, or the data key does not unwrap.
    #[error(transparent)]
    Read(#[from] ReadError),
    /// Encrypting the rendered bytes failed.
    #[error("sealing the {kind} of blob {blob_id}: {source}")]
    Seal {
        /// The representation being stored.
        kind: Representation,
        /// Its blob.
        blob_id: Uuid,
        /// Why encryption failed.
        source: agrafe_crypto::Error,
    },
    /// The stored object does not decrypt: a wrong key, or a tampered or
    /// truncated object.
    #[error("the {kind} of blob {blob_id} is corrupt: {source}")]
    Corrupt {
        /// The representation being loaded.
        kind: Representation,
        /// Its blob.
        blob_id: Uuid,
        /// Why decryption failed.
        source: agrafe_crypto::Error,
    },
    /// The object store failed writing the rendered object.
    #[error("writing the {kind} of blob {blob_id}: {source}")]
    Put {
        /// The representation being stored.
        kind: Representation,
        /// Its blob.
        blob_id: Uuid,
        /// Why the write failed.
        source: object_store::Error,
    },
    /// The object store failed reading the rendered object back, for a
    /// reason other than its absence.
    #[error("reading the {kind} of blob {blob_id}: {source}")]
    Get {
        /// The representation being loaded.
        kind: Representation,
        /// Its blob.
        blob_id: Uuid,
        /// Why the read failed.
        source: object_store::Error,
    },
    /// The query queueing a render failed.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

/// What a request for a representation resolves to.
#[derive(Debug)]
pub enum Resolved {
    /// Rendered. The digest and size the row records answer a conditional
    /// or `HEAD` request on their own; the bytes are an
    /// `Agrafe::load_representation` away.
    Ready(Rendered),
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

    /// The bytes of a `kind` that resolved `Ready`, or `None` when the
    /// object is gone: the representation is then set back to `pending`
    /// and queued again, and the caller serves the placeholder.
    pub async fn load_representation(
        &self,
        blob: &BlobHead,
        kind: Representation,
    ) -> Result<Option<Bytes>, Error> {
        match load(self, &Stored::from_head(blob)?, kind).await? {
            Some(bytes) => Ok(Some(bytes)),
            None => {
                tracing::warn!(blob = %blob.id, kind = %kind, "object is missing, regenerating");
                repo::request_representation(self.pool(), blob.id, kind).await?;
                self.heads().invalidate(blob.id);
                Ok(None)
            }
        }
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
        state.heads().invalidate(blob.id);
        return Ok(Resolved::Placeholder(RepresentationState::Pending));
    }
    match rep_state {
        RepresentationState::Ready => match blob.rendered(kind) {
            Some(rendered) => Ok(Resolved::Ready(rendered)),
            None => {
                tracing::warn!(blob = %blob.id, kind = %kind, "ready without a digest, regenerating");
                repo::request_representation(state.pool(), blob.id, kind).await?;
                state.heads().invalidate(blob.id);
                Ok(Resolved::Placeholder(RepresentationState::Pending))
            }
        },
        other => Ok(Resolved::Placeholder(other)),
    }
}

/// Encrypt `plaintext` and write it as the `kind` of the blob `stored`
/// describes, replacing any previous version. What the row should record
/// of it comes back.
pub async fn store(
    state: &Agrafe,
    stored: &Stored,
    kind: Representation,
    plaintext: &[u8],
) -> Result<Rendered, Error> {
    let blob_id = stored.blob_id;
    let key = state
        .keks()
        .unwrap(blob_id, &stored.kek_id, &stored.wrapped_key)
        .map_err(ReadError::from)?;
    let object_key = kind.object_key(blob_id);
    let sealed = streaming::encrypt(
        &key,
        super::key::associated_data(&object_key).as_bytes(),
        stored.layout,
        plaintext,
    )
    .map_err(|source| Error::Seal {
        kind,
        blob_id,
        source,
    })?;
    state
        .store()
        .put(&ObjectPath::from(object_key), sealed.into())
        .await
        .map_err(|source| Error::Put {
            kind,
            blob_id,
            source,
        })?;
    Ok(Rendered {
        sha256: Sha256::digest(plaintext).into(),
        size: plaintext.len() as u64,
    })
}

/// Read and decrypt the `kind` of the blob `stored` describes, or `None`
/// when the object is not there.
pub async fn load(
    state: &Agrafe,
    stored: &Stored,
    kind: Representation,
) -> Result<Option<Bytes>, Error> {
    let blob_id = stored.blob_id;
    let object_key = kind.object_key(blob_id);
    let get = |source| Error::Get {
        kind,
        blob_id,
        source,
    };
    let sealed = match state
        .store()
        .get(&ObjectPath::from(object_key.as_str()))
        .await
    {
        Ok(result) => result.bytes().await.map_err(get)?,
        Err(object_store::Error::NotFound { .. }) => return Ok(None),
        Err(err) => return Err(get(err)),
    };
    let key = state
        .keks()
        .unwrap(blob_id, &stored.kek_id, &stored.wrapped_key)
        .map_err(ReadError::from)?;
    // Opened where it was fetched; the store hands the buffer over whole,
    // and a shared one is copied first.
    let mut buffer = sealed
        .try_into_mut()
        .unwrap_or_else(|shared| BytesMut::from(&shared[..]));
    let len = streaming::decrypt_in_place(
        &key,
        super::key::associated_data(&object_key).as_bytes(),
        stored.layout,
        &mut buffer,
    )
    .map_err(|source| Error::Corrupt {
        kind,
        blob_id,
        source,
    })?;
    buffer.truncate(len);
    Ok(Some(buffer.freeze()))
}
