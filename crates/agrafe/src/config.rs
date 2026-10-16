//! Configuration: one TOML file plus `AGRAFE_` environment overrides.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use bytesize::ByteSize;
use figment::Figment;
use figment::providers::{Env, Format, Toml};
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

use crate::jobs::JobKind;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub storage: StorageConfig,
    pub signing: SigningConfig,
    #[serde(default)]
    pub upload: UploadConfig,
    #[serde(default)]
    pub encryption: EncryptionConfig,
    #[serde(default)]
    pub clamav: ClamavConfig,
    #[serde(default)]
    pub media: MediaConfig,
    #[serde(default)]
    pub jobs: JobsConfig,
}

impl Config {
    /// Load from `path` (if it exists) and the environment.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut figment = Figment::new();
        if let Some(path) = path {
            figment = figment.merge(Toml::file_exact(path));
        }
        let config: Config = figment
            .merge(Env::prefixed("AGRAFE_").split("__"))
            .extract()
            .context("invalid configuration")?;
        config.validate()?;
        Ok(config)
    }

    /// Cross-field checks that no single section owns. Signing keys are
    /// checked by `Signer::from_config`, which is the only consumer.
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.server.public_base_url.ends_with('/'),
            "server.public_base_url must not end with a slash"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub private_addr: SocketAddr,
    pub public_addr: SocketAddr,
    pub public_base_url: String,
    #[serde(default)]
    pub role: Role,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Api,
    Worker,
    #[default]
    All,
}

impl Role {
    pub fn runs_api(self) -> bool {
        matches!(self, Role::Api | Role::All)
    }
    pub fn runs_worker(self) -> bool {
        matches!(self, Role::Worker | Role::All)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    /// Carries the password, so it is a `Secret`.
    pub url: Secret,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

fn default_max_connections() -> u32 {
    10
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum StorageConfig {
    Local { local: LocalStorageConfig },
    S3 { s3: S3StorageConfig },
}

#[derive(Debug, Clone, Deserialize)]
pub struct LocalStorageConfig {
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct S3StorageConfig {
    pub bucket: String,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<Secret>,
}

/// A string that must not appear in logs: redacted by `Debug`, read through
/// `expose`.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// A secret given as base64 in config.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Base64Secret(String);

impl Base64Secret {
    pub fn decoded(&self) -> Result<Vec<u8>> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(self.0.trim())
            .context("secret is not valid base64")
    }
}

impl From<String> for Base64Secret {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Debug for Base64Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Base64Secret(…)")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SigningConfig {
    pub current_key: String,
    pub keys: HashMap<String, Base64Secret>,
    #[serde(with = "humantime_serde", default = "default_window")]
    pub window: Duration,
    #[serde(default)]
    pub allow_permanent: bool,
    #[serde(default)]
    pub ttl: SigningTtl,
}

fn default_window() -> Duration {
    Duration::from_secs(3600)
}

#[derive(Debug, Clone, Deserialize)]
pub struct SigningTtl {
    #[serde(with = "humantime_serde", default = "default_upload_ttl")]
    pub upload: Duration,
    #[serde(with = "humantime_serde", default = "default_read_ttl")]
    pub metadata: Duration,
    #[serde(with = "humantime_serde", default = "default_read_ttl")]
    pub read: Duration,
}

fn default_upload_ttl() -> Duration {
    Duration::from_secs(3600)
}
fn default_read_ttl() -> Duration {
    Duration::from_secs(24 * 3600)
}

impl Default for SigningTtl {
    fn default() -> Self {
        Self {
            upload: default_upload_ttl(),
            metadata: default_read_ttl(),
            read: default_read_ttl(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct UploadConfig {
    #[serde(default = "default_max_size")]
    pub max_size: ByteSize,
    #[serde(default = "default_inline_scan_max_size")]
    pub inline_scan_max_size: ByteSize,
    #[serde(with = "humantime_serde", default = "default_stale_after")]
    pub stale_after: Duration,
    /// Grace period before the object a delete released is purged from the
    /// bucket: a download that started before the delete keeps its bytes.
    #[serde(with = "humantime_serde", default = "default_purge_delay")]
    pub purge_delay: Duration,
}

impl UploadConfig {
    /// When an object released at `now` may be purged.
    pub fn purge_at(&self, now: Timestamp) -> Timestamp {
        SignedDuration::try_from(self.purge_delay)
            .ok()
            .and_then(|d| now.checked_add(d).ok())
            .unwrap_or(Timestamp::MAX)
    }
}

fn default_max_size() -> ByteSize {
    ByteSize::gib(5)
}
fn default_inline_scan_max_size() -> ByteSize {
    ByteSize::mib(25)
}
fn default_stale_after() -> Duration {
    Duration::from_secs(3600)
}
fn default_purge_delay() -> Duration {
    Duration::from_secs(3600)
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            max_size: default_max_size(),
            inline_scan_max_size: default_inline_scan_max_size(),
            stale_after: default_stale_after(),
            purge_delay: default_purge_delay(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct EncryptionConfig {
    #[serde(default)]
    pub current_kek: Option<String>,
    #[serde(default)]
    pub keks: HashMap<String, Base64Secret>,
    #[serde(default = "default_chunk_size")]
    pub chunk_size: ByteSize,
}

fn default_chunk_size() -> ByteSize {
    ByteSize::kib(256)
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClamavConfig {
    #[serde(default = "default_clamav_addr")]
    pub addr: String,
    #[serde(with = "humantime_serde", default = "default_clamav_timeout")]
    pub timeout: Duration,
}

fn default_clamav_addr() -> String {
    "127.0.0.1:3310".to_string()
}
fn default_clamav_timeout() -> Duration {
    Duration::from_secs(60)
}

impl Default for ClamavConfig {
    fn default() -> Self {
        Self {
            addr: default_clamav_addr(),
            timeout: default_clamav_timeout(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MediaConfig {
    #[serde(default = "default_variant_version")]
    pub variant_version: i32,
    #[serde(default)]
    pub thumbnail: ThumbnailConfig,
    #[serde(default)]
    pub preview: PreviewConfig,
    #[serde(default)]
    pub embedded_image: EmbeddedImageConfig,
}

fn default_variant_version() -> i32 {
    1
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            variant_version: default_variant_version(),
            thumbnail: ThumbnailConfig::default(),
            preview: PreviewConfig::default(),
            embedded_image: EmbeddedImageConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThumbnailConfig {
    pub width: u32,
    pub height: u32,
    pub quality: u8,
}

impl Default for ThumbnailConfig {
    fn default() -> Self {
        Self {
            width: 256,
            height: 256,
            quality: 80,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewConfig {
    pub max_dimension: u32,
    pub quality: u8,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            max_dimension: 2048,
            quality: 85,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddedImageConfig {
    pub min_coverage: f32,
    pub max_text_chars: u32,
}

impl Default for EmbeddedImageConfig {
    fn default() -> Self {
        Self {
            min_coverage: 0.7,
            max_text_chars: 50,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobsConfig {
    #[serde(with = "humantime_serde", default = "default_poll_interval")]
    pub poll_interval: Duration,
    #[serde(with = "humantime_serde", default = "default_lease_timeout")]
    pub lease_timeout: Duration,
    #[serde(default)]
    pub concurrency: HashMap<String, usize>,
}

impl JobsConfig {
    /// Workers for `kind`: the configured count, else a default suited to
    /// what the kind waits on.
    pub fn concurrency_for(&self, kind: JobKind) -> usize {
        if let Some(n) = self.concurrency.get(kind.as_str()) {
            return *n;
        }
        match kind {
            JobKind::Scan | JobKind::Purge => 4,
            JobKind::Thumbnail | JobKind::Preview => 2,
            JobKind::Reap => 1,
        }
    }
}

fn default_poll_interval() -> Duration {
    Duration::from_secs(2)
}
fn default_lease_timeout() -> Duration {
    Duration::from_secs(600)
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            poll_interval: default_poll_interval(),
            lease_timeout: default_lease_timeout(),
            concurrency: HashMap::new(),
        }
    }
}
