//! Key-encryption keys from configuration and the per-blob envelope.

use std::collections::HashMap;

use agrafe_crypto::streaming::Layout;
use agrafe_crypto::wrap::Kek;
use agrafe_crypto::{Error as CryptoError, Key};
use anyhow::{Context, Result};
use uuid::Uuid;

use crate::config::EncryptionConfig;

/// Every KEK in config, one of them current for wrapping new data keys.
pub struct Keks {
    keks: HashMap<String, Kek>,
    current: String,
    layout: Layout,
}

/// A freshly wrapped data key and where it came from.
pub struct Envelope {
    pub key: Key,
    pub kek_id: String,
    pub wrapped: Vec<u8>,
}

impl Keks {
    pub fn from_config(config: &EncryptionConfig) -> Result<Self> {
        let mut keks = HashMap::new();
        for (id, secret) in &config.keks {
            anyhow::ensure!(!id.is_empty(), "encryption KEK ids must not be empty");
            let bytes = secret
                .decoded()
                .with_context(|| format!("encryption KEK `{id}`"))?;
            let key = Key::from_slice(&bytes)
                .with_context(|| format!("encryption KEK `{id}` must be 32 bytes"))?;
            keks.insert(id.clone(), Kek::new(&key));
        }
        let current = config
            .current_kek
            .clone()
            .context("encryption.current_kek is required")?;
        anyhow::ensure!(
            keks.contains_key(&current),
            "encryption.current_kek `{current}` is not present in encryption.keks"
        );
        let chunk_size = usize::try_from(config.chunk_size.as_u64())
            .ok()
            .and_then(|s| Layout::new(s).ok())
            .context("encryption.chunk_size is out of range")?;
        Ok(Self {
            keks,
            current,
            layout: chunk_size,
        })
    }

    /// The segment layout new objects are written with.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Generate a data key for `blob_id` and wrap it under the current KEK.
    pub fn new_envelope(&self, blob_id: Uuid) -> Envelope {
        let key = Key::generate();
        let wrapped = self.keks[&self.current].wrap(&key, wrap_aad(blob_id).as_bytes());
        Envelope {
            key,
            kek_id: self.current.clone(),
            wrapped,
        }
    }

    /// Unwrap a stored data key.
    pub fn unwrap(&self, blob_id: Uuid, kek_id: &str, wrapped: &[u8]) -> Result<Key> {
        let kek = self
            .keks
            .get(kek_id)
            .with_context(|| format!("blob {blob_id} is wrapped with unknown KEK `{kek_id}`"))?;
        kek.unwrap(wrapped, wrap_aad(blob_id).as_bytes())
            .map_err(|e: CryptoError| {
                anyhow::anyhow!("unwrapping the data key of blob {blob_id}: {e}")
            })
    }
}

/// The wrapped key is bound to its blob so it cannot be moved to another row.
fn wrap_aad(blob_id: Uuid) -> String {
    blob_id.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Base64Secret;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use bytesize::ByteSize;

    fn config(current: Option<&str>) -> EncryptionConfig {
        let mut keks = HashMap::new();
        keks.insert(
            "a".to_string(),
            Base64Secret::from(STANDARD.encode([1u8; 32])),
        );
        keks.insert(
            "b".to_string(),
            Base64Secret::from(STANDARD.encode([2u8; 32])),
        );
        EncryptionConfig {
            current_kek: current.map(String::from),
            keks,
            chunk_size: ByteSize::kib(64),
        }
    }

    #[test]
    fn wraps_and_unwraps_with_any_known_kek() {
        let keks = Keks::from_config(&config(Some("a"))).unwrap();
        let id = Uuid::now_v7();
        let env = keks.new_envelope(id);
        assert_eq!(env.kek_id, "a");
        assert_eq!(keks.unwrap(id, "a", &env.wrapped).unwrap(), env.key);
        assert!(keks.unwrap(Uuid::now_v7(), "a", &env.wrapped).is_err());
        assert!(keks.unwrap(id, "b", &env.wrapped).is_err());
        assert!(keks.unwrap(id, "zz", &env.wrapped).is_err());
    }

    #[test]
    fn validates_config() {
        assert!(Keks::from_config(&config(None)).is_err());
        assert!(Keks::from_config(&config(Some("zz"))).is_err());
        let mut short = config(Some("a"));
        short
            .keks
            .insert("a".into(), Base64Secret::from(STANDARD.encode([1u8; 16])));
        assert!(Keks::from_config(&short).is_err());
        let mut tiny = config(Some("a"));
        tiny.chunk_size = ByteSize::b(16);
        assert!(Keks::from_config(&tiny).is_err());
    }
}
