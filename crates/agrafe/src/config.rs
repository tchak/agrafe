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
    #[serde(default)]
    pub archive: ArchiveConfig,
    #[serde(default)]
    pub blobs: BlobsConfig,
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
        if self.jobs.kinds.as_ref().is_some_and(Vec::is_empty) {
            return Err(ConfigError::Invalid(
                "jobs.kinds must name at least one kind, or be omitted for all".into(),
            ));
        }
        if self.blobs.default_expires_in < BlobsConfig::MIN_EXPIRES_IN {
            return Err(ConfigError::Invalid(format!(
                "blobs.default_expires_in must be at least {}: a blob created without an \
                 expiry has to outlive the form its upload is attached from",
                humantime_serde::re::humantime::format_duration(BlobsConfig::MIN_EXPIRES_IN)
            )));
        }
        if self.archive.max_size.as_u64() > ArchiveConfig::MAX_SIZE_LIMIT {
            return Err(ConfigError::Invalid(format!(
                "archive.max_size must be at most {} bytes: the build writes 32 MiB parts, \
                 the object store allows 10 000 of them, and the encryption and the \
                 archive's own records need a margin on top of the estimate",
                ArchiveConfig::MAX_SIZE_LIMIT
            )));
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Api,
    Worker,
    #[default]
    All,
}

agrafe_types::str_enum!(Role {
    Api => "api",
    Worker => "worker",
    All => "all",
});

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
    /// A `created` blob whose last source part is older than this is an
    /// abandoned export: the reaper expires it and its parts are purged.
    #[serde(with = "humantime_serde", default = "default_source_stale_after")]
    pub source_stale_after: Duration,
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
    Duration::from_secs(600)
}
fn default_purge_delay() -> Duration {
    Duration::from_secs(3600)
}
fn default_source_stale_after() -> Duration {
    Duration::from_secs(24 * 3600)
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            max_size: default_max_size(),
            inline_scan_max_size: default_inline_scan_max_size(),
            stale_after: default_stale_after(),
            purge_delay: default_purge_delay(),
            source_stale_after: default_source_stale_after(),
        }
    }
}

/// The lifetime of blobs and of their tombstones. See DESIGN.md,
/// "Attachments" and "State machine".
#[derive(Debug, Clone, Deserialize)]
pub struct BlobsConfig {
    /// Expiry of a blob created without one, and of a blob whose last
    /// attachment was removed, counted from that moment. A blob is either
    /// expiring or attached; there is no way to create one that is neither.
    #[serde(with = "humantime_serde", default = "default_expires_in")]
    pub default_expires_in: Duration,
    /// How long a terminal blob's row stays after its objects were purged,
    /// or after it went terminal with nothing to purge, so that the backend
    /// can tell a blob it deleted from an id that never existed. The reaper
    /// removes it after, with its attachments and jobs.
    #[serde(with = "humantime_serde", default = "default_keep_purged")]
    pub keep_purged: Duration,
    /// How long the read routes keep a settled row they served, so a blob
    /// read many times costs one query per process per period; zero keeps
    /// nothing. See `blob::cache`.
    #[serde(with = "humantime_serde", default = "default_read_cache_ttl")]
    pub read_cache_ttl: Duration,
    /// Rows kept at most.
    #[serde(default = "default_read_cache_entries")]
    pub read_cache_entries: u64,
}

impl BlobsConfig {
    /// The floor on `default_expires_in`: a direct upload is attached when
    /// its form is submitted, and a form takes as long as its user takes.
    pub const MIN_EXPIRES_IN: Duration = Duration::from_secs(3600);

    /// When a blob left without an expiry at `now` expires.
    pub fn default_expires_at(&self, now: Timestamp) -> Timestamp {
        SignedDuration::try_from(self.default_expires_in)
            .ok()
            .and_then(|d| now.checked_add(d).ok())
            .unwrap_or(Timestamp::MAX)
    }
}

fn default_expires_in() -> Duration {
    Duration::from_secs(24 * 3600)
}
fn default_keep_purged() -> Duration {
    Duration::from_secs(90 * 24 * 3600)
}
fn default_read_cache_ttl() -> Duration {
    Duration::from_secs(30)
}
fn default_read_cache_entries() -> u64 {
    100_000
}

impl Default for BlobsConfig {
    fn default() -> Self {
        Self {
            default_expires_in: default_expires_in(),
            keep_purged: default_keep_purged(),
            read_cache_ttl: default_read_cache_ttl(),
            read_cache_entries: default_read_cache_entries(),
        }
    }
}

/// Limits on generated archives. See DESIGN.md, "Archives".
#[derive(Debug, Clone, Deserialize)]
pub struct ArchiveConfig {
    /// Upper bound on the size of a generated archive, estimated at accept
    /// time from the members' sizes. A limit of its own: `upload.max_size`
    /// is sized for one request, an archive for a whole export.
    #[serde(default = "default_archive_max_size")]
    pub max_size: ByteSize,
}

impl ArchiveConfig {
    /// What the build can write at most: 10 000 parts of 32 MiB, less one
    /// percent. The limit is checked against the accept-time estimate of
    /// the plaintext, while the parts carry the ciphertext, a tag per
    /// segment longer, plus `ERRORS.txt` and the end records the estimate
    /// leaves out; without the margin an archive at the limit would need a
    /// part the store refuses. About 309 GiB.
    pub const MAX_SIZE_LIMIT: u64 =
        crate::jobs::build::MAX_BUILD_PARTS * crate::jobs::build::BUILD_PART_SIZE as u64 / 100 * 99;
}

fn default_archive_max_size() -> ByteSize {
    ByteSize::gib(100)
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            max_size: default_archive_max_size(),
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
    /// Originals above this size get no representations, except the text
    /// thumbnail, which reads only the first few KiB.
    #[serde(default = "default_max_source_size")]
    pub max_source_size: ByteSize,
    /// Where the workers unpack originals while rendering; the system
    /// temporary directory by default.
    #[serde(default)]
    pub tmp_dir: Option<PathBuf>,
    /// The tools, boxes and sandbox limits, owned by the render crate and
    /// read from the same `[media]` table: `vips_command`,
    /// `extract_command`, `render_text_command`, `thumbnail`, `preview`,
    /// `embedded_image`, `sandbox`.
    #[serde(flatten)]
    pub render: agrafe_render::Config,
}

fn default_variant_version() -> i32 {
    1
}
fn default_max_source_size() -> ByteSize {
    ByteSize::mib(64)
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            variant_version: default_variant_version(),
            max_source_size: default_max_source_size(),
            tmp_dir: None,
            render: agrafe_render::Config::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobsConfig {
    #[serde(with = "humantime_serde", default = "default_poll_interval")]
    pub poll_interval: Duration,
    #[serde(with = "humantime_serde", default = "default_lease_timeout")]
    pub lease_timeout: Duration,
    /// How often the reaper runs.
    #[serde(with = "humantime_serde", default = "default_reap_interval")]
    pub reap_interval: Duration,
    /// How long a dead job stays in the table before the reaper prunes it.
    #[serde(with = "humantime_serde", default = "default_keep_failed")]
    pub keep_failed: Duration,
    #[serde(default)]
    /// Keyed by kind, so an unknown name is a configuration error rather
    /// than a silently ignored line.
    pub concurrency: HashMap<JobKind, usize>,
    /// The kinds this process runs workers for; every kind when omitted.
    /// A deployment must cover each kind with some process.
    pub kinds: Option<Vec<JobKind>>,
}

impl JobsConfig {
    /// Workers for `kind`: the configured count, else a default suited to
    /// what the kind waits on.
    pub fn concurrency_for(&self, kind: JobKind) -> usize {
        if let Some(n) = self.concurrency.get(&kind) {
            return *n;
        }
        match kind {
            JobKind::Scan | JobKind::Render | JobKind::Purge => 4,
            JobKind::Rows => 2,
            JobKind::Archive | JobKind::Reap => 1,
        }
    }

    /// The kinds this process handles, in the order of `JobKind::ALL`, each
    /// once.
    pub fn kinds(&self) -> Vec<JobKind> {
        match &self.kinds {
            None => JobKind::ALL.to_vec(),
            Some(kinds) => JobKind::ALL
                .iter()
                .copied()
                .filter(|k| kinds.contains(k))
                .collect(),
        }
    }

    pub fn handles(&self, kind: JobKind) -> bool {
        self.kinds
            .as_ref()
            .is_none_or(|kinds| kinds.contains(&kind))
    }
}

fn default_poll_interval() -> Duration {
    Duration::from_secs(2)
}
fn default_lease_timeout() -> Duration {
    Duration::from_secs(600)
}
fn default_reap_interval() -> Duration {
    Duration::from_secs(60)
}
fn default_keep_failed() -> Duration {
    Duration::from_secs(7 * 24 * 3600)
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            poll_interval: default_poll_interval(),
            lease_timeout: default_lease_timeout(),
            reap_interval: default_reap_interval(),
            keep_failed: default_keep_failed(),
            concurrency: HashMap::new(),
            kinds: None,
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
    fn the_default_expiry_has_a_floor() {
        Jail::expect_with(|jail| {
            jail.set_env("AGRAFE_BLOBS__DEFAULT_EXPIRES_IN", "1h");
            assert_eq!(
                load(&example())?.blobs.default_expires_in,
                BlobsConfig::MIN_EXPIRES_IN
            );
            for below in ["0s", "59m"] {
                jail.set_env("AGRAFE_BLOBS__DEFAULT_EXPIRES_IN", below);
                let err = load(&example()).unwrap_err().to_string();
                assert!(err.contains("must be at least 1h"), "{below}: {err}");
            }
            Ok(())
        });
    }

    #[test]
    fn media_tools_read_from_the_flattened_section() {
        Jail::expect_with(|jail| {
            let config = load(&example())?;
            assert_eq!(config.media.render.vips_command, "vips");
            assert_eq!(config.media.render.extract_command, "agrafe-extract");
            assert_eq!(config.media.render.sandbox.timeout.as_secs(), 60);
            assert_eq!(config.media.render.thumbnail.width, 256);

            jail.set_env("AGRAFE_MEDIA__VIPS_COMMAND", "/opt/vips/bin/vips");
            jail.set_env("AGRAFE_MEDIA__SANDBOX__TIMEOUT", "5s");
            jail.set_env("AGRAFE_MEDIA__MAX_SOURCE_SIZE", "1 MiB");
            let config = load(&example())?;
            assert_eq!(config.media.render.vips_command, "/opt/vips/bin/vips");
            assert_eq!(config.media.render.sandbox.timeout.as_secs(), 5);
            assert_eq!(config.media.max_source_size.as_u64(), 1 << 20);
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
    fn kinds_select_the_workers_a_process_runs() {
        let cfg = jobs("").unwrap();
        assert_eq!(cfg.kinds(), JobKind::ALL);
        assert!(cfg.handles(JobKind::Archive));

        let cfg = jobs(r#"kinds = ["archive", "scan", "scan"]"#).unwrap();
        assert_eq!(cfg.kinds(), [JobKind::Scan, JobKind::Archive]);
        assert!(!cfg.handles(JobKind::Render));
        assert!(jobs(r#"kinds = ["scann"]"#).is_none(), "a typo is an error");

        Jail::expect_with(|jail| {
            jail.set_env("AGRAFE_JOBS__KINDS", r#"["render", "rows"]"#);
            let config = load(&example())?;
            assert_eq!(config.jobs.kinds(), [JobKind::Render, JobKind::Rows]);
            jail.set_env("AGRAFE_JOBS__KINDS", "[]");
            let err = load(&example()).unwrap_err().to_string();
            assert!(err.contains("at least one kind"), "{err}");
            Ok(())
        });
    }

    #[test]
    fn archive_max_size_is_bounded_by_the_part_count() {
        Jail::expect_with(|jail| {
            let config = load(&example())?;
            assert_eq!(config.archive.max_size, ByteSize::gib(100));
            assert_eq!(config.upload.source_stale_after.as_secs(), 24 * 3600);
            jail.set_env("AGRAFE_ARCHIVE__MAX_SIZE", "400 GiB");
            let err = load(&example()).unwrap_err().to_string();
            assert!(err.contains("archive.max_size"), "{err}");
            // 10 000 parts of 32 MiB is 312.5 GiB; the margin takes it
            // just under 310.
            jail.set_env("AGRAFE_ARCHIVE__MAX_SIZE", "310 GiB");
            assert!(load(&example()).is_err());
            jail.set_env("AGRAFE_ARCHIVE__MAX_SIZE", "309 GiB");
            assert_eq!(load(&example())?.archive.max_size, ByteSize::gib(309));
            Ok(())
        });
    }

    #[test]
    fn concurrency_is_keyed_by_kind() {
        let cfg = jobs("[concurrency]\nscan = 7").unwrap();
        assert_eq!(cfg.concurrency_for(JobKind::Scan), 7);
        assert_eq!(cfg.concurrency_for(JobKind::Render), 4, "default");
        assert!(
            jobs("[concurrency]\nscann = 7").is_none(),
            "a typo is an error"
        );
    }
}
