//! Configuration: one TOML file plus `AGRAFE_` environment overrides.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytesize::ByteSize;
use figment::Figment;
use figment::providers::{Env, Format, Toml};
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

use crate::jobs::JobKind;

/// A configuration that cannot be loaded, or that loads but cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid configuration: {0}")]
    Load(Box<figment::Error>),
    #[error("{0}")]
    Invalid(String),
}

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
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut figment = Figment::new();
        if let Some(path) = path {
            figment = figment.merge(Toml::file_exact(path));
        }
        let config: Config = figment
            .merge(Env::prefixed("AGRAFE_").split("__"))
            .extract()
            .map_err(|e| ConfigError::Load(Box::new(e)))?;
        config.validate()?;
        Ok(config)
    }

    /// Cross-field checks that no single section owns. Signing keys are
    /// checked by `Signer::from_config`, which is the only consumer.
    fn validate(&self) -> Result<(), ConfigError> {
        if self.server.public_base_url.ends_with('/') {
            return Err(ConfigError::Invalid(
                "server.public_base_url must not end with a slash".into(),
            ));
        }
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
    pub fn decoded(&self) -> Result<Vec<u8>, base64::DecodeError> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(self.0.trim())
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
    /// For the TCP connect alone. A clamd that drops packets rather than
    /// refusing them must not hold every small upload for `timeout`.
    #[serde(with = "humantime_serde", default = "default_clamav_connect_timeout")]
    pub connect_timeout: Duration,
    /// Inline scans hold a clamd connection for as long as the uploader
    /// takes; this caps how many at once. Beyond it the scan is deferred.
    #[serde(default = "default_max_inline_scans")]
    pub max_inline_scans: usize,
}

fn default_clamav_addr() -> String {
    "127.0.0.1:3310".to_string()
}
fn default_clamav_timeout() -> Duration {
    Duration::from_secs(60)
}
fn default_clamav_connect_timeout() -> Duration {
    Duration::from_secs(5)
}
fn default_max_inline_scans() -> usize {
    4
}

impl Default for ClamavConfig {
    fn default() -> Self {
        Self {
            addr: default_clamav_addr(),
            timeout: default_clamav_timeout(),
            connect_timeout: default_clamav_connect_timeout(),
            max_inline_scans: default_max_inline_scans(),
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
    /// The libvips command line tool, run as a sandboxed subprocess.
    #[serde(default = "default_vips_command")]
    pub vips_command: String,
    /// The agrafe binary the workers re-execute for embedded image
    /// extraction. Defaults to the running executable.
    #[serde(default)]
    pub agrafe_command: Option<PathBuf>,
    /// Originals above this size get no representations.
    #[serde(default = "default_max_source_size")]
    pub max_source_size: ByteSize,
    /// Where the workers unpack originals while rendering; the system
    /// temporary directory by default.
    #[serde(default)]
    pub tmp_dir: Option<PathBuf>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
}

fn default_variant_version() -> i32 {
    1
}
fn default_vips_command() -> String {
    "vips".to_string()
}
fn default_max_source_size() -> ByteSize {
    ByteSize::mib(512)
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            variant_version: default_variant_version(),
            thumbnail: ThumbnailConfig::default(),
            preview: PreviewConfig::default(),
            embedded_image: EmbeddedImageConfig::default(),
            vips_command: default_vips_command(),
            agrafe_command: None,
            max_source_size: default_max_source_size(),
            tmp_dir: None,
            sandbox: SandboxConfig::default(),
        }
    }
}

/// Limits on the subprocesses that decode and render user files.
#[derive(Debug, Clone, Deserialize)]
pub struct SandboxConfig {
    /// Wall-clock limit; the process is killed past it.
    #[serde(with = "humantime_serde", default = "default_sandbox_timeout")]
    pub timeout: Duration,
    /// Address space limit (`RLIMIT_AS`).
    #[serde(default = "default_sandbox_memory")]
    pub memory: ByteSize,
    /// Largest file the process may write (`RLIMIT_FSIZE`).
    #[serde(default = "default_sandbox_max_output_size")]
    pub max_output_size: ByteSize,
}

fn default_sandbox_timeout() -> Duration {
    Duration::from_secs(60)
}
fn default_sandbox_memory() -> ByteSize {
    ByteSize::gib(1)
}
fn default_sandbox_max_output_size() -> ByteSize {
    ByteSize::mib(256)
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            timeout: default_sandbox_timeout(),
            memory: default_sandbox_memory(),
            max_output_size: default_sandbox_max_output_size(),
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
    /// Keyed by kind, so an unknown name is a configuration error rather
    /// than a silently ignored line.
    pub concurrency: HashMap<JobKind, usize>,
}

impl JobsConfig {
    /// Workers for `kind`: the configured count, else a default suited to
    /// what the kind waits on.
    pub fn concurrency_for(&self, kind: JobKind) -> usize {
        if let Some(n) = self.concurrency.get(&kind) {
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

#[cfg(test)]
#[expect(
    clippy::result_large_err,
    reason = "figment::Jail closures return figment::Error"
)]
mod tests {
    use figment::providers::{Format, Toml};
    use figment::{Figment, Jail};

    use super::*;

    /// The documented reference configuration.
    fn example() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/example.toml")
    }

    fn load(path: &Path) -> figment::error::Result<Config> {
        Config::load(Some(path)).map_err(|e| e.to_string().into())
    }

    #[test]
    fn example_config_loads() {
        Jail::expect_with(|_| {
            let config = load(&example())?;
            assert_eq!(config.server.role, Role::All);
            assert_eq!(config.signing.current_key, "k1");
            assert_eq!(config.jobs.concurrency_for(JobKind::Scan), 4);
            Ok(())
        });
    }

    #[test]
    fn environment_overrides_the_file() {
        Jail::expect_with(|jail| {
            jail.set_env(
                "AGRAFE_DATABASE__URL",
                "postgres://agrafe:hunter2@db/agrafe",
            );
            jail.set_env("AGRAFE_JOBS__CONCURRENCY__SCAN", "9");
            let config = load(&example())?;
            assert_eq!(
                config.database.url.expose(),
                "postgres://agrafe:hunter2@db/agrafe"
            );
            assert_eq!(config.jobs.concurrency_for(JobKind::Scan), 9);

            jail.set_env("AGRAFE_SERVER__PUBLIC_BASE_URL", "https://files.example/");
            let err = load(&example()).unwrap_err().to_string();
            assert!(err.contains("must not end with a slash"), "{err}");
            Ok(())
        });
    }

    #[test]
    fn secrets_stay_out_of_debug() {
        Jail::expect_with(|jail| {
            jail.set_env(
                "AGRAFE_DATABASE__URL",
                "postgres://agrafe:hunter2@db/agrafe",
            );
            let debug = format!("{:?}", load(&example())?);
            assert!(!debug.contains("hunter2"), "{debug}");
            // The signing key and the KEK placeholders.
            assert!(!debug.contains("REPLACE_ME"), "{debug}");
            assert!(debug.contains("Secret(…)"), "{debug}");
            Ok(())
        });
    }

    fn jobs(toml: &str) -> Option<JobsConfig> {
        Figment::from(Toml::string(toml)).extract().ok()
    }

    #[test]
    fn concurrency_is_keyed_by_kind() {
        let cfg = jobs("[concurrency]\nscan = 7").unwrap();
        assert_eq!(cfg.concurrency_for(JobKind::Scan), 7);
        assert_eq!(cfg.concurrency_for(JobKind::Preview), 2, "default");
        assert!(
            jobs("[concurrency]\nscann = 7").is_none(),
            "a typo is an error"
        );
    }
}
