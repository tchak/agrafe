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
use super::{
    BlobHead, BlobState, ImageFormat, Object, Rendered, Representation, RepresentationState,
    WebpFallback, repo,
};
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
    /// Rendered, with the WebP fallback stored beside the picture when
    /// there is one. The digest and size the row records answer a
    /// conditional or `HEAD` request on their own; the bytes are an
    /// `Agrafe::load_representation` away.
    Ready(Rendered, Option<WebpFallback>),
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

    /// The bytes of a `kind` an envelope records `ready`, the picture or
    /// its WebP fallback, or `None` when the object is gone or does not
    /// open. Touches nothing else: the sealed path has not looked at the
    /// row and hands that over to it.
    pub async fn read_representation(
        &self,
        blob: &BlobHead,
        kind: Representation,
        object: Object,
    ) -> Result<Option<Bytes>, Error> {
        match load(self, &Stored::from_head(blob)?, kind, object).await {
            Err(Error::Corrupt { .. }) => Ok(None),
            loaded => loaded,
        }
    }

    /// The bytes of a `kind` that resolved `Ready`, the picture or its
    /// WebP fallback, or `None` when the object is gone or does not open:
    /// a representation can be rendered again, so it is set back to
    /// `pending` and queued, and the caller serves the placeholder.
    pub async fn load_representation(
        &self,
        blob: &BlobHead,
        kind: Representation,
        object: Object,
    ) -> Result<Option<Bytes>, Error> {
        match load(self, &Stored::from_head(blob)?, kind, object).await {
            Ok(Some(bytes)) => return Ok(Some(bytes)),
            Ok(None) => {
                tracing::warn!(blob = %blob.id, kind = %kind, ?object, "object is missing, regenerating");
            }
            Err(err @ Error::Corrupt { .. }) => {
                tracing::error!(error = %err, "regenerating");
            }
            Err(err) => return Err(err),
        }
        repo::request_representation(self.pool(), blob.id, kind).await?;
        self.heads().invalidate(blob.id);
        Ok(None)
    }
}

async fn resolve(state: &Agrafe, blob: &BlobHead, kind: Representation) -> Result<Resolved, Error> {
    let now = Timestamp::now();
    if !blob.is_readable(now) {
        return Ok(Resolved::NotReady(blob.state_at(now)));
    }
    let media = &state.config().media;
    let rep_state = blob.representations.state(kind);
    if !matches!(
        rep_state,
        RepresentationState::Pending | RepresentationState::Disabled
    ) && blob.representations.version(kind) != Some(media.variant_version)
    {
        return regenerate(state, blob, kind, "built with another variant version").await;
    }
    match rep_state {
        RepresentationState::Ready => match blob.representations.rendered(kind) {
            Some(rendered) => {
                let fallback = blob.representations.fallback(kind);
                if lacks_fallback(&media.render, rendered, fallback) {
                    let why = "without the WebP fallback the deployment stores";
                    return regenerate(state, blob, kind, why).await;
                }
                Ok(Resolved::Ready(rendered, fallback))
            }
            // The row's CHECKs rule this out. Still, nothing is served
            // without the digest its headers are made from.
            None => regenerate(state, blob, kind, "ready without a digest").await,
        },
        other => Ok(Resolved::Placeholder(other)),
    }
}

/// Set `kind` back to `pending` and queue a render, `why` being what is
/// wrong with what the row has, and hand the caller the placeholder.
async fn regenerate(
    state: &Agrafe,
    blob: &BlobHead,
    kind: Representation,
    why: &str,
) -> Result<Resolved, Error> {
    tracing::info!(blob = %blob.id, kind = %kind, "{why}, regenerating");
    repo::request_representation(state.pool(), blob.id, kind).await?;
    state.heads().invalidate(blob.id);
    Ok(Resolved::Placeholder(RepresentationState::Pending))
}

/// Whether a `ready` picture lacks the WebP fallback this deployment
/// stores beside its pictures (`media.webp_fallback`): one rendered before
/// the setting was turned on, which would go to the browsers the fallback
/// is for as it is. Such a picture is regenerated on request, as one of
/// another variant version is, so turning the fallback on takes no bump.
/// A WebP picture needs none and is served as it is.
pub fn lacks_fallback(
    render: &agrafe_render::Config,
    rendered: Rendered,
    fallback: Option<WebpFallback>,
) -> bool {
    render.webp_fallback && rendered.format != ImageFormat::Webp && fallback.is_none()
}

/// Encrypt `plaintext`, a picture in `format`, and write it as the `kind`
/// of the blob `stored` describes, replacing any previous version. What
/// the row should record of it comes back.
pub async fn store(
    state: &Agrafe,
    stored: &Stored,
    kind: Representation,
    format: ImageFormat,
    plaintext: &[u8],
) -> Result<Rendered, Error> {
    put(state, stored, kind, Object::Picture, plaintext).await?;
    Ok(Rendered {
        format,
        sha256: Sha256::digest(plaintext).into(),
        size: plaintext.len() as u64,
    })
}

/// Encrypt `plaintext`, the WebP fallback of the `kind` picture, and write
/// it beside the picture, at `<kind>.webp`, replacing any previous one.
/// What the row should record of it comes back.
pub async fn store_fallback(
    state: &Agrafe,
    stored: &Stored,
    kind: Representation,
    plaintext: &[u8],
) -> Result<WebpFallback, Error> {
    put(state, stored, kind, Object::Fallback, plaintext).await?;
    Ok(WebpFallback {
        sha256: Sha256::digest(plaintext).into(),
        size: plaintext.len() as u64,
    })
}

/// Encrypt `plaintext` with the blob's key, bound to the object's name,
/// and write it there.
async fn put(
    state: &Agrafe,
    stored: &Stored,
    kind: Representation,
    object: Object,
    plaintext: &[u8],
) -> Result<(), Error> {
    let blob_id = stored.blob_id;
    let key = state
        .keks()
        .unwrap(blob_id, &stored.kek_id, &stored.wrapped_key)
        .map_err(ReadError::from)?;
    let object_key = super::key::object(blob_id, kind, object);
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
    Ok(())
}

/// Read and decrypt the `object` of the `kind` of the blob `stored`
/// describes, or `None` when it is not there.
pub async fn load(
    state: &Agrafe,
    stored: &Stored,
    kind: Representation,
    object: Object,
) -> Result<Option<Bytes>, Error> {
    let blob_id = stored.blob_id;
    let object_key = super::key::object(blob_id, kind, object);
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
