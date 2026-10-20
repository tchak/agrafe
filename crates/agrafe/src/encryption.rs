//! Key-encryption keys from configuration and the per-blob envelope.

use std::collections::HashMap;
use std::path::Path;

use agrafe_crypto::streaming::Layout;
use agrafe_crypto::wrap::Kek;
use agrafe_crypto::{Error as CryptoError, Key};
use uuid::Uuid;

use crate::config::{Base64Secret, ConfigError, EncryptionConfig};

/// Upper bound on `encryption.chunk_size`. A segment is held in memory whole
/// on both the write and the read path, once per in-flight request.
pub const MAX_CHUNK_SIZE: u64 = 16 * 1024 * 1024;

/// A stored data key that cannot be unwrapped.
#[derive(Debug, thiserror::Error)]
pub enum UnwrapError {
    /// The blob's key was wrapped under a KEK no longer, or never, in
    /// `encryption.keks`.
    #[error("blob {blob_id} is wrapped with unknown KEK `{kek_id}`")]
    UnknownKek {
        /// The blob whose key it is.
        blob_id: Uuid,
        /// The KEK id stored with the wrapped key.
        kek_id: String,
    },
    /// The KEK is known but the wrapped key does not open under it: the
    /// key bytes are damaged, the KEK under that id changed, or the key was
    /// wrapped for another blob.
    #[error("unwrapping the data key of blob {blob_id}: {source}")]
    Crypto {
        /// The blob whose key it is.
        blob_id: Uuid,
        /// Why unwrapping failed.
        source: CryptoError,
    },
}

/// Every KEK in config, one of them current for wrapping new data keys.
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub struct Keks {
    keks: HashMap<String, Kek>,
    current: String,
    layout: Layout,
}

/// A freshly wrapped data key and where it came from.
pub(crate) struct Envelope {
    /// The data key in clear, the blob's objects are encrypted with.
    pub key: Key,
    /// The id of the KEK it was wrapped under, the current one.
    pub kek_id: String,
    /// The data key wrapped under that KEK, bound to its blob, as stored
    /// in the blob's row.
    pub wrapped: Vec<u8>,
}

impl Keks {
    /// The KEKs of `encryption.keks` and of the file `encryption.keks_file`
    /// names, which is read here, once, and never again.
    pub(crate) fn from_config(config: &EncryptionConfig) -> Result<Self, ConfigError> {
        let invalid = ConfigError::Invalid;
        let mut keks = HashMap::new();
        for (id, secret) in &config.keks {
            keks.insert(id.clone(), parse_kek(id, secret)?);
        }
        if let Some(path) = &config.keks_file {
            for (id, secret) in read_keks_file(path)? {
                if keks.contains_key(&id) {
                    return Err(invalid(format!(
                        "encryption KEK `{id}` is given both in encryption.keks and in \
                         encryption.keks_file"
                    )));
                }
                let kek = parse_kek(&id, &secret)?;
                keks.insert(id, kek);
            }
        }
        let current = config
            .current_kek
            .clone()
            .ok_or_else(|| invalid("encryption.current_kek is required".into()))?;
        if !keks.contains_key(&current) {
            return Err(invalid(format!(
                "encryption.current_kek `{current}` is not present in encryption.keks or \
                 encryption.keks_file"
            )));
        }
        if config.chunk_size.as_u64() > MAX_CHUNK_SIZE {
            return Err(invalid(format!(
                "encryption.chunk_size must be at most {MAX_CHUNK_SIZE} bytes (16 MiB)"
            )));
        }
        let chunk_size = usize::try_from(config.chunk_size.as_u64())
            .ok()
            .and_then(|s| Layout::new(s).ok())
            .ok_or_else(|| invalid("encryption.chunk_size is too small".into()))?;
        Ok(Self {
            keks,
            current,
            layout: chunk_size,
        })
    }

    /// The segment layout new objects are written with.
    pub(crate) fn layout(&self) -> Layout {
        self.layout
    }

    /// Generate a data key for `blob_id` and wrap it under the current KEK.
    pub(crate) fn new_envelope(&self, blob_id: Uuid) -> Envelope {
        let key = Key::generate();
        let wrapped = self.keks[&self.current].wrap(&key, wrap_aad(blob_id).as_bytes());
        Envelope {
            key,
            kek_id: self.current.clone(),
            wrapped,
        }
    }

    /// Unwrap a stored data key.
    pub fn unwrap(&self, blob_id: Uuid, kek_id: &str, wrapped: &[u8]) -> Result<Key, UnwrapError> {
        let kek = self
            .keks
            .get(kek_id)
            .ok_or_else(|| UnwrapError::UnknownKek {
                blob_id,
                kek_id: kek_id.to_string(),
            })?;
        kek.unwrap(wrapped, wrap_aad(blob_id).as_bytes())
            .map_err(|source| UnwrapError::Crypto { blob_id, source })
    }
}

/// The KEK `secret` stands for, or why it is not one.
fn parse_kek(id: &str, secret: &Base64Secret) -> Result<Kek, ConfigError> {
    let invalid = ConfigError::Invalid;
    if id.is_empty() {
        return Err(invalid("encryption KEK ids must not be empty".into()));
    }
    let bytes = secret
        .decoded()
        .map_err(|e| invalid(format!("encryption KEK `{id}` is not valid base64: {e}")))?;
    let key = Key::from_slice(&bytes)
        .map_err(|_| invalid(format!("encryption KEK `{id}` must be 32 bytes")))?;
    Ok(Kek::new(&key))
}

/// The KEKs in the file at `path`: a TOML table of id to base64 key, the
/// body of `[encryption.keks]`. Read to its end in one go, so a FIFO works:
/// the read blocks until the writer closes it. Nothing re-reads it later.
///
/// A malformed file is reported by line, never by content: the error ends
/// up in the journal, and the content is a key, most likely one a human
/// just mistyped at a prompt.
fn read_keks_file(path: &Path) -> Result<HashMap<String, Base64Secret>, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        ConfigError::Invalid(format!("encryption.keks_file `{}`: {e}", path.display()))
    })?;
    toml::from_str(&text).map_err(|e| {
        let line = e
            .span()
            .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1);
        let at = line.map_or(String::new(), |line| format!(" at line {line}"));
        ConfigError::Invalid(format!(
            "encryption.keks_file `{}` must be a TOML table of KEK id to base64 key, \
             `id = \"…\"` lines: {}{at}",
            path.display(),
            e.message()
        ))
    })
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
            keks_file: None,
            chunk_size: ByteSize::kib(64),
        }
    }

    fn keks_file(contents: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), contents).unwrap();
        file
    }

    #[test]
    fn reads_keks_from_a_file_alongside_the_inline_ones() {
        let file = keks_file(&format!(
            "c = \"{}\"\nd = \"{}\"\n",
            STANDARD.encode([3u8; 32]),
            STANDARD.encode([4u8; 32])
        ));
        let mut config = config(Some("c"));
        config.keks_file = Some(file.path().to_path_buf());
        let keks = Keks::from_config(&config).unwrap();
        let id = Uuid::now_v7();
        let env = keks.new_envelope(id);
        assert_eq!(env.kek_id, "c");
        assert_eq!(keks.unwrap(id, "c", &env.wrapped).unwrap(), env.key);
        // The inline ones are still there.
        assert!(keks.unwrap(id, "a", &env.wrapped).is_err());
        assert!(matches!(
            keks.unwrap(id, "zz", &env.wrapped),
            Err(UnwrapError::UnknownKek { .. })
        ));

        // The file alone is enough.
        config.keks.clear();
        assert!(Keks::from_config(&config).is_ok());
    }

    // The documented deployment: a FIFO the operator fills after the
    // service has started. The read must block until the writer closes it
    // and must not be retried, or the second open would hang forever.
    #[cfg(unix)]
    #[test]
    fn reads_keks_from_a_fifo_filled_after_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keks");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(200));
                std::fs::write(&path, format!("c = \"{}\"\n", STANDARD.encode([3u8; 32]))).unwrap();
            })
        };
        let mut config = config(Some("c"));
        config.keks_file = Some(path);
        let keks = Keks::from_config(&config).unwrap();
        writer.join().unwrap();
        let id = Uuid::now_v7();
        let env = keks.new_envelope(id);
        assert_eq!(env.kek_id, "c");
        assert_eq!(keks.unwrap(id, "c", &env.wrapped).unwrap(), env.key);
    }

    #[test]
    fn validates_the_keks_file() {
        let err = |config: &EncryptionConfig| match Keks::from_config(config) {
            Ok(_) => panic!("accepted"),
            Err(e) => e.to_string(),
        };

        let mut config = config(Some("a"));
        config.keks_file = Some("/nonexistent/keks.toml".into());
        assert!(err(&config).starts_with("encryption.keks_file `/nonexistent/keks.toml`:"));

        let file = keks_file("not toml at all [");
        config.keks_file = Some(file.path().to_path_buf());
        assert!(err(&config).contains("must be a TOML table of KEK id to base64 key"));

        let file = keks_file("c = 42\n");
        config.keks_file = Some(file.path().to_path_buf());
        assert!(err(&config).contains("must be a TOML table of KEK id to base64 key"));

        // A syntax error names the line, never its content: a key mistyped
        // at the prompt must not land in the journal.
        let kek = STANDARD.encode([9u8; 32]);
        for (broken, line) in [
            (format!("c = \"{kek}\n"), 1),
            (format!("c = {kek}\n"), 1),
            (format!("c = \"{kek}\" x\n"), 1),
            (format!("b = \"{kek}\"\n\nc = \"{kek}\"\n{{{kek}}}\n"), 4),
        ] {
            let file = keks_file(&broken);
            config.keks_file = Some(file.path().to_path_buf());
            let message = err(&config);
            assert!(!message.contains(&kek[..8]), "{message}");
            assert!(message.ends_with(&format!(" at line {line}")), "{message}");
        }

        let file = keks_file(&format!("a = \"{}\"\n", STANDARD.encode([9u8; 32])));
        config.keks_file = Some(file.path().to_path_buf());
        assert_eq!(
            err(&config),
            "encryption KEK `a` is given both in encryption.keks and in encryption.keks_file"
        );

        let file = keks_file(&format!("c = \"{}\"\n", STANDARD.encode([9u8; 16])));
        config.keks_file = Some(file.path().to_path_buf());
        assert_eq!(err(&config), "encryption KEK `c` must be 32 bytes");

        let file = keks_file("");
        config.keks_file = Some(file.path().to_path_buf());
        config.current_kek = Some("c".into());
        assert_eq!(
            err(&config),
            "encryption.current_kek `c` is not present in encryption.keks or encryption.keks_file"
        );
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
        let mut huge = config(Some("a"));
        huge.chunk_size = ByteSize::mib(16) + ByteSize::b(1);
        assert!(Keks::from_config(&huge).is_err());
        let mut max = config(Some("a"));
        max.chunk_size = ByteSize::mib(16);
        assert!(Keks::from_config(&max).is_ok());
    }
}
