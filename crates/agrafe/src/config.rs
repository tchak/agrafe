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
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::jobs::JobKind;
use crate::jobs::notify::Event;

/// A configuration that cannot be loaded, or that loads but cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file or the environment could not be read or parsed into a
    /// `Config`: a syntax error, an unknown variant, a missing key.
    #[error("invalid configuration: {0}")]
    Load(Box<figment::Error>),
    /// The configuration parsed but a value is unusable, alone or with
    /// another; the message names the key and why.
    #[error("{0}")]
    Invalid(String),
}

/// The whole configuration, one field per table of the file. See
/// `config/example.toml` for every key.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// `[server]`: the two interfaces and the role. Required.
    pub server: ServerConfig,
    /// `[database]`: the Postgres connection. Required.
    pub database: DatabaseConfig,
    /// `[storage]`: where the objects are kept. Required.
    pub storage: StorageConfig,
    /// `[signing]`: the keys and lifetimes of public URLs. Required.
    pub signing: SigningConfig,
    /// `[upload]`: size caps and the timings of uploads in flight.
    #[serde(default)]
    pub upload: UploadConfig,
    /// `[encryption]`: the key-encryption keys objects are sealed under.
    #[serde(default)]
    pub encryption: EncryptionConfig,
    /// `[clamav]`: the clamd that scans uploads.
    #[serde(default)]
    pub clamav: ClamavConfig,
    /// `[media]`: the derived representations and the tools that render
    /// them.
    #[serde(default)]
    pub media: MediaConfig,
    /// `[jobs]`: the queue's timings and the workers this process runs.
    #[serde(default)]
    pub jobs: JobsConfig,
    /// `[archive]`: limits on generated archives.
    #[serde(default)]
    pub archive: ArchiveConfig,
    /// `[document]`: limits on generated documents.
    #[serde(default)]
    pub document: DocumentConfig,
    /// `[blobs]`: expiries, tombstones and the read cache.
    #[serde(default)]
    pub blobs: BlobsConfig,
    /// `[fetch]`: how `url` blobs are downloaded.
    #[serde(default)]
    pub fetch: FetchConfig,
    /// `[dashboard]`: the HTML dashboard, off by default.
    #[serde(default)]
    pub dashboard: DashboardConfig,
    /// `[webhook]`: where state changes are posted, none by default.
    #[serde(default)]
    pub webhook: WebhookConfig,
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
        if let Some(host) = self
            .server
            .private_hosts
            .iter()
            .find(|h| h.is_empty() || h.contains('/') || h.contains("://"))
        {
            return Err(ConfigError::Invalid(format!(
                "server.private_hosts: {host:?} is not a host and port, like agrafe.internal:8080"
            )));
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
        if self.media.variant_version < 1 {
            // Zero stands for "none" in the envelope, so a zero version
            // would never match and every sealed representation would fall
            // back to the row.
            return Err(ConfigError::Invalid(
                "media.variant_version must be at least 1".into(),
            ));
        }
        self.webhook.validate()?;
        self.media
            .render
            .geojson
            .validate()
            .map_err(ConfigError::Invalid)?;
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

/// The two interfaces and what this process runs.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Where the private interface listens: no signatures, trusted network
    /// only. A worker-only process binds it too, for `/healthz` and
    /// `/metrics` alone.
    pub private_addr: SocketAddr,
    /// Where the public interface listens, every URL on it signed.
    pub public_addr: SocketAddr,
    /// The base public URLs are minted under, as clients reach the public
    /// interface (`https://files.example`). No trailing slash.
    pub public_base_url: String,
    /// Which parts of the service this process runs; `all` by default. The
    /// `--role` flag takes precedence.
    #[serde(default)]
    pub role: Role,
    /// The names and ports the private interface is reached under
    /// (`agrafe.internal:8080`). Set, a private request whose `Host` is
    /// none of them is refused, which is what stops DNS rebinding from
    /// turning a browser on the trusted network into a client of the
    /// port; see [DESIGN.md, "Dashboard"]. Empty, no check.
    ///
    /// [DESIGN.md, "Dashboard"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#dashboard
    #[serde(default)]
    pub private_hosts: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
/// The parts of the service a process runs, `server.role`.
pub enum Role {
    /// The HTTP interfaces, no workers.
    Api,
    /// The job workers, with the private interface for `/healthz` and
    /// `/metrics` alone.
    Worker,
    /// Both, the default.
    #[default]
    All,
}

agrafe_types::str_enum!(Role {
    Api => "api",
    Worker => "worker",
    All => "all",
});

impl Role {
    /// Whether this role serves the HTTP interfaces: `api` or `all`.
    pub fn runs_api(self) -> bool {
        matches!(self, Role::Api | Role::All)
    }
    /// Whether this role runs the job workers: `worker` or `all`.
    pub fn runs_worker(self) -> bool {
        matches!(self, Role::Worker | Role::All)
    }
}

/// The Postgres connection.
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    /// The connection URL, `postgres://…`. Carries the password, so it is
    /// a `Secret`.
    pub url: Secret,
    /// Connections the pool opens at most; 10 by default.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

fn default_max_connections() -> u32 {
    10
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
/// The object store, chosen by `storage.backend`, with the settings of
/// that backend in the table of the same name.
pub enum StorageConfig {
    /// A directory on the local filesystem, `backend = "local"`.
    Local {
        /// `[storage.local]`.
        local: LocalStorageConfig,
    },
    /// An S3-compatible bucket, `backend = "s3"`.
    S3 {
        /// `[storage.s3]`.
        s3: S3StorageConfig,
    },
    /// An Azure Blob Storage container, `backend = "azure"`.
    Azure {
        /// `[storage.azure]`.
        azure: AzureStorageConfig,
    },
    /// A Google Cloud Storage bucket, `backend = "gcs"`.
    Gcs {
        /// `[storage.gcs]`.
        gcs: GcsStorageConfig,
    },
}

/// Objects as files under a directory.
#[derive(Debug, Clone, Deserialize)]
pub struct LocalStorageConfig {
    /// The directory the objects are kept under.
    pub path: String,
}

/// An S3-compatible bucket. It needs an `AbortIncompleteMultipartUpload`
/// lifecycle rule: a crashed process cannot abort the multipart uploads it
/// had open.
#[derive(Debug, Clone, Deserialize)]
pub struct S3StorageConfig {
    /// The bucket's name.
    pub bucket: String,
    /// The bucket's region (`fr-par`); `None` leaves it to the `AWS_*`
    /// environment variables.
    pub region: Option<String>,
    /// The endpoint of a provider other than AWS
    /// (`https://s3.fr-par.scw.cloud`); `None` leaves it to the
    /// environment, AWS by default.
    pub endpoint: Option<String>,
    /// The access key id; `None` leaves the credentials to the `AWS_*`
    /// environment variables. Prefer `AGRAFE_STORAGE__S3__ACCESS_KEY_ID`.
    pub access_key_id: Option<String>,
    /// The secret paired with `access_key_id`. Prefer
    /// `AGRAFE_STORAGE__S3__SECRET_ACCESS_KEY`.
    pub secret_access_key: Option<Secret>,
}

/// An Azure Blob Storage container. Azure discards uncommitted blocks after
/// seven days, so no lifecycle rule is needed.
#[derive(Debug, Clone, Deserialize)]
pub struct AzureStorageConfig {
    /// The storage account's name.
    pub account: String,
    /// The container's name.
    pub container: String,
    /// The account's shared key; `None` falls back to the `AZURE_*`
    /// environment variables or a managed identity. Prefer
    /// `AGRAFE_STORAGE__AZURE__ACCESS_KEY`.
    pub access_key: Option<Secret>,
    /// An endpoint other than Azure's public one, for Azurite or a
    /// sovereign cloud.
    pub endpoint: Option<String>,
}

/// A Google Cloud Storage bucket. It needs an
/// `AbortIncompleteMultipartUpload` lifecycle rule, as on S3.
#[derive(Debug, Clone, Deserialize)]
pub struct GcsStorageConfig {
    /// The bucket's name.
    pub bucket: String,
    /// The JSON key of a service account, not a path to it; `None` falls
    /// back to `GOOGLE_SERVICE_ACCOUNT`, the application default
    /// credentials or the metadata server.
    pub service_account_key: Option<Secret>,
}

/// A string that must not appear in logs: redacted by `Debug`, read through
/// `expose`.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// The secret in clear, for the one place that needs it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// A secret given as base64 in config. Wiped when dropped, as are the bytes
/// it decodes to.
#[derive(Clone, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct Base64Secret(String);

impl Base64Secret {
    /// The bytes the base64 stands for, surrounding whitespace ignored, or
    /// the error that it is not base64.
    pub fn decoded(&self) -> Result<Zeroizing<Vec<u8>>, base64::DecodeError> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(self.0.trim())
            .map(Zeroizing::new)
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

/// The keys and lifetimes of signed public URLs.
#[derive(Debug, Clone, Deserialize)]
pub struct SigningConfig {
    /// The id, in `keys`, of the key new tokens are minted with.
    pub current_key: String,
    /// Every key a token may be verified with, by id: the current one and
    /// those being rotated out. Ids appear in tokens, so letters, digits,
    /// `-` and `_` only; values are at least 32 random bytes in base64.
    pub keys: HashMap<String, Base64Secret>,
    /// Expiries are rounded up to the next multiple of this before the TTL
    /// is added, so URLs minted inside one window are identical and
    /// cacheable; an hour by default.
    #[serde(with = "humantime_serde", default = "default_window")]
    pub window: Duration,
    /// `[signing.ttl]`: how long each kind of URL lives.
    #[serde(default)]
    pub ttl: SigningTtl,
}

fn default_window() -> Duration {
    Duration::from_secs(3600)
}

/// How long minted URLs live, by kind.
#[derive(Debug, Clone, Deserialize)]
pub struct SigningTtl {
    /// Upload URLs; an hour by default.
    #[serde(with = "humantime_serde", default = "default_upload_ttl")]
    pub upload: Duration,
    /// Metadata URLs; a day by default.
    #[serde(with = "humantime_serde", default = "default_read_ttl")]
    pub metadata: Duration,
    /// Read URLs, the original and its representations; a day by default.
    #[serde(with = "humantime_serde", default = "default_read_ttl")]
    pub read: Duration,
    /// The longest any URL may live: a ceiling on the TTLs above, on the
    /// override `POST /blobs/:id/urls` accepts, and on the expiry of any
    /// token presented to the public port. A week by default.
    #[serde(with = "humantime_serde", default = "default_max_ttl")]
    pub max: Duration,
}

fn default_upload_ttl() -> Duration {
    Duration::from_secs(3600)
}
fn default_read_ttl() -> Duration {
    Duration::from_secs(24 * 3600)
}
fn default_max_ttl() -> Duration {
    Duration::from_secs(7 * 24 * 3600)
}

impl Default for SigningTtl {
    fn default() -> Self {
        Self {
            upload: default_upload_ttl(),
            metadata: default_read_ttl(),
            read: default_read_ttl(),
            max: default_max_ttl(),
        }
    }
}

/// Limits and timings of uploads.
#[derive(Debug, Clone, Deserialize)]
pub struct UploadConfig {
    /// Hard cap on any upload; 5 GiB by default.
    #[serde(default = "default_max_size")]
    pub max_size: ByteSize,
    /// Files up to this size are scanned inline as they are uploaded;
    /// larger ones are stored first and scanned by a job. 25 MiB by
    /// default.
    #[serde(default = "default_inline_scan_max_size")]
    pub inline_scan_max_size: ByteSize,
    /// An upload in flight touches its blob every third of this; one left
    /// untouched this long was abandoned by a crashed process, and the
    /// reaper resets it. Ten minutes by default.
    #[serde(with = "humantime_serde", default = "default_stale_after")]
    pub stale_after: Duration,
    /// Grace period before the object a delete released is purged from the
    /// bucket: a download that started before the delete keeps its bytes.
    /// An hour by default.
    #[serde(with = "humantime_serde", default = "default_purge_delay")]
    pub purge_delay: Duration,
    /// A `created` blob whose last source part is older than this is an
    /// abandoned export, and an attached `created` blob with no parts that
    /// nothing has touched for this long is a file that never came: the
    /// reaper expires both, parts purged, attachments kept. A day by
    /// default.
    #[serde(with = "humantime_serde", default = "default_abandoned_after")]
    pub abandoned_after: Duration,
}

impl UploadConfig {
    /// When an object released at `now` may be purged.
    pub(crate) fn purge_at(&self, now: Timestamp) -> Timestamp {
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
fn default_abandoned_after() -> Duration {
    Duration::from_secs(24 * 3600)
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            max_size: default_max_size(),
            inline_scan_max_size: default_inline_scan_max_size(),
            stale_after: default_stale_after(),
            purge_delay: default_purge_delay(),
            abandoned_after: default_abandoned_after(),
        }
    }
}

/// The lifetime of blobs and of their tombstones. See [DESIGN.md,
/// "Attachments"] and ["State machine"].
///
/// [DESIGN.md, "Attachments"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#attachments
/// ["State machine"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#state-machine
#[derive(Debug, Clone, Deserialize)]
pub struct BlobsConfig {
    /// Expiry of a blob created without one, and of a blob whose last
    /// attachment was removed, counted from that moment. A blob is either
    /// expiring or attached; there is no way to create one that is neither.
    /// A day by default, at least `MIN_EXPIRES_IN`.
    #[serde(with = "humantime_serde", default = "default_expires_in")]
    pub default_expires_in: Duration,
    /// How long a terminal blob's row stays after its objects were purged,
    /// or after it went terminal with nothing to purge, so that the backend
    /// can tell a blob it deleted from an id that never existed. The reaper
    /// removes it after, with its attachments and jobs. 90 days by
    /// default.
    #[serde(with = "humantime_serde", default = "default_keep_purged")]
    pub keep_purged: Duration,
    /// How long the read routes keep a settled row they served, so a blob
    /// read many times costs one query per process per period; zero keeps
    /// nothing. 30 seconds by default. See `blob::cache`.
    #[serde(with = "humantime_serde", default = "default_read_cache_ttl")]
    pub read_cache_ttl: Duration,
    /// Rows the read cache keeps at most, per process; 100 000 by default.
    #[serde(default = "default_read_cache_entries")]
    pub read_cache_entries: u64,
}

impl BlobsConfig {
    /// The floor on `default_expires_in`: a direct upload is attached when
    /// its form is submitted, and a form takes as long as its user takes.
    pub const MIN_EXPIRES_IN: Duration = Duration::from_secs(3600);

    /// When a blob left without an expiry at `now` expires.
    pub(crate) fn default_expires_at(&self, now: Timestamp) -> Timestamp {
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

/// Limits on generated archives. See [DESIGN.md, "Archives"].
///
/// [DESIGN.md, "Archives"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#archives
#[derive(Debug, Clone, Deserialize)]
pub struct ArchiveConfig {
    /// Upper bound on the size of a generated archive, estimated at accept
    /// time from the members' sizes. A limit of its own: `upload.max_size`
    /// is sized for one request, an archive for a whole export. 100 GiB by
    /// default, at most `MAX_SIZE_LIMIT`.
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

/// Limits on generated documents. See [DESIGN.md, "Documents"].
///
/// [DESIGN.md, "Documents"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#documents
#[derive(Debug, Clone, Deserialize)]
pub struct DocumentConfig {
    /// Upper bound on the bytes a document's workspace takes once written
    /// out for the compiler, the members' sizes plus the inline files,
    /// checked at accept time and again by the build. A limit of its own:
    /// `upload.max_size` is sized for one request, and the workspace is
    /// written to the worker's disk. 256 MiB by default.
    #[serde(default = "default_document_max_workspace_size")]
    pub max_workspace_size: ByteSize,
    /// Upper bound on the files of a workspace, the manifest's lines across
    /// its parts, checked at accept time and again by the build. Bytes
    /// alone do not bound a workspace: an empty file costs none, and a
    /// manifest of a few million of them costs the worker as many files
    /// on disk. 10 000 by default.
    #[serde(default = "default_document_max_files")]
    pub max_files: u64,
}

fn default_document_max_workspace_size() -> ByteSize {
    ByteSize::mib(256)
}

fn default_document_max_files() -> u64 {
    10_000
}

impl Default for DocumentConfig {
    fn default() -> Self {
        Self {
            max_workspace_size: default_document_max_workspace_size(),
            max_files: default_document_max_files(),
        }
    }
}

/// How `url` blobs are fetched. See [DESIGN.md, "Fetched blobs"].
///
/// [DESIGN.md, "Fetched blobs"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#fetched-blobs
#[derive(Debug, Clone, Deserialize)]
pub struct FetchConfig {
    /// The TCP connect and the TLS handshake; 10 seconds by default.
    #[serde(with = "humantime_serde", default = "default_fetch_connect_timeout")]
    pub connect_timeout: Duration,
    /// The longest a read of the response may wait for the next bytes. The
    /// stall detector: a remote that trickles cannot hold a fetch worker,
    /// while a large file on a slow link gets as long as it keeps flowing.
    /// 30 seconds by default.
    #[serde(with = "humantime_serde", default = "default_fetch_read_timeout")]
    pub read_timeout: Duration,
    /// The whole response, headers and body: a backstop behind
    /// `read_timeout`, generous enough for the largest file. A fetch that
    /// runs past it fails the attempt and is retried from the start. Six
    /// hours by default.
    #[serde(with = "humantime_serde", default = "default_fetch_timeout")]
    pub timeout: Duration,
    /// Redirects followed at most, every hop under the same checks; 5 by
    /// default.
    #[serde(default = "default_max_redirects")]
    pub max_redirects: usize,
    /// A `Retry-After` beyond this is not waited for: the fetch is dead at
    /// once rather than parked. 15 minutes by default.
    #[serde(with = "humantime_serde", default = "default_max_retry_after")]
    pub max_retry_after: Duration,
    /// Allow URLs on loopback, private, link-local and the other refused
    /// ranges, and plain `http`: for local development and the tests,
    /// never in production.
    #[serde(default)]
    pub allow_private_networks: bool,
    /// The `User-Agent` sent with every fetch; `agrafe/` and the crate's
    /// version by default.
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
}

/// The HTML dashboard on the private interface. See [DESIGN.md,
/// "Dashboard"]. Both off by default: a deployment does not grow an
/// interface by upgrading.
///
/// [DESIGN.md, "Dashboard"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#dashboard
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DashboardConfig {
    /// Serve it under `/dashboard` on the private interface.
    #[serde(default)]
    pub enabled: bool,
    /// Show the retry and discard buttons. Off, the dashboard is read-only
    /// and the action routes answer `404`.
    #[serde(default)]
    pub actions: bool,
}

fn default_fetch_connect_timeout() -> Duration {
    Duration::from_secs(10)
}
fn default_fetch_read_timeout() -> Duration {
    Duration::from_secs(30)
}
fn default_fetch_timeout() -> Duration {
    Duration::from_secs(6 * 3600)
}
fn default_max_redirects() -> usize {
    5
}
fn default_max_retry_after() -> Duration {
    Duration::from_secs(15 * 60)
}
fn default_user_agent() -> String {
    concat!("agrafe/", env!("CARGO_PKG_VERSION")).to_string()
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            connect_timeout: default_fetch_connect_timeout(),
            read_timeout: default_fetch_read_timeout(),
            timeout: default_fetch_timeout(),
            max_redirects: default_max_redirects(),
            max_retry_after: default_max_retry_after(),
            allow_private_networks: false,
            user_agent: default_user_agent(),
        }
    }
}

/// Where state changes are posted. See [DESIGN.md, "Webhooks"].
///
/// [DESIGN.md, "Webhooks"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#webhooks
#[derive(Debug, Clone, Deserialize)]
pub struct WebhookConfig {
    /// The one URL the deliveries go to. No webhook when unset: nothing
    /// is enqueued.
    pub url: Option<String>,
    /// The signing secret, `whsec_` followed by base64, as Standard
    /// Webhooks hands it to a receiver. Required with `url`.
    pub secret: Option<Secret>,
    /// The whole request, connect to the last byte of the response;
    /// anything but a `2xx` within it fails the attempt. 10 seconds by
    /// default.
    #[serde(with = "humantime_serde", default = "default_webhook_timeout")]
    pub timeout: Duration,
    /// The events delivered; every event when omitted.
    pub events: Option<Vec<Event>>,
    /// Accept an `http` URL. The body carries a `url` blob's presigned
    /// source and the blob's envelope, so the default is `https` only.
    #[serde(default)]
    pub allow_insecure: bool,
}

impl WebhookConfig {
    /// Whether `event` is to be delivered: a URL is set and the event is
    /// not filtered out.
    pub(crate) fn wants(&self, event: Event) -> bool {
        self.url.is_some()
            && self
                .events
                .as_ref()
                .is_none_or(|events| events.contains(&event))
    }

    /// The key the signature is computed with: the secret decoded.
    pub(crate) fn key(&self) -> Result<Vec<u8>, ConfigError> {
        use base64::Engine;
        let secret = self
            .secret
            .as_ref()
            .map(Secret::expose)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ConfigError::Invalid("webhook.secret is required with webhook.url".into())
            })?;
        let encoded = secret.strip_prefix("whsec_").ok_or_else(|| {
            ConfigError::Invalid(
                "webhook.secret must be whsec_ followed by base64: whsec_ and the output of \
                 openssl rand -base64 24"
                    .into(),
            )
        })?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|err| ConfigError::Invalid(format!("webhook.secret is not base64: {err}")))
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let Some(url) = &self.url else {
            return Ok(());
        };
        let parsed = url::Url::parse(url)
            .map_err(|err| ConfigError::Invalid(format!("webhook.url is not a URL: {err}")))?;
        match parsed.scheme() {
            "https" => {}
            "http" if self.allow_insecure => {}
            "http" => {
                return Err(ConfigError::Invalid(
                    "webhook.url must be https: the body carries secrets; set \
                     webhook.allow_insecure = true to post over http"
                        .into(),
                ));
            }
            _ => {
                return Err(ConfigError::Invalid(
                    "webhook.url must be http or https".into(),
                ));
            }
        }
        self.key()?;
        if self.events.as_ref().is_some_and(Vec::is_empty) {
            return Err(ConfigError::Invalid(
                "webhook.events must name at least one event, or be omitted for all".into(),
            ));
        }
        Ok(())
    }
}

fn default_webhook_timeout() -> Duration {
    Duration::from_secs(10)
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            url: None,
            secret: None,
            timeout: default_webhook_timeout(),
            events: None,
            allow_insecure: false,
        }
    }
}

/// The key-encryption keys each blob's data key is wrapped under, and the
/// segment size of the streaming AEAD.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct EncryptionConfig {
    /// The id, in `keks`, of the key new blobs are wrapped under. Optional
    /// in the file, required to start: `Agrafe::new` fails without it.
    #[serde(default)]
    pub current_kek: Option<String>,
    /// Every key a wrapped data key may be opened with, by id: the current
    /// one and those being rotated out. Values are 32 random bytes in
    /// base64.
    #[serde(default)]
    pub keks: HashMap<String, Base64Secret>,
    /// A file holding more KEKs in the same shape as `keks`, `id = "base64"`
    /// lines, read once at startup and never again. It may be a FIFO or a
    /// systemd credential, so the KEKs need not sit on the host's disk. An
    /// id in both places is an error. Relative to the working directory.
    #[serde(default)]
    pub keks_file: Option<PathBuf>,
    /// The streaming AEAD segment size, at most 16 MiB: a segment is held
    /// in memory whole per request in flight. Stored per blob, so it can
    /// change later. 256 KiB by default.
    #[serde(default = "default_chunk_size")]
    pub chunk_size: ByteSize,
}

fn default_chunk_size() -> ByteSize {
    ByteSize::kib(256)
}

/// The clamd that scans uploads. When it cannot be reached, uploads still
/// succeed and their scan is deferred to a job.
#[derive(Debug, Clone, Deserialize)]
pub struct ClamavConfig {
    /// clamd's TCP socket, a host and port; `127.0.0.1:3310` by default.
    #[serde(default = "default_clamav_addr")]
    pub addr: String,
    /// Bounds each write and the wait for the verdict, not a whole scan;
    /// 60 seconds by default.
    #[serde(with = "humantime_serde", default = "default_clamav_timeout")]
    pub timeout: Duration,
    /// For the TCP connect alone. A clamd that drops packets rather than
    /// refusing them must not hold every small upload for `timeout`. Five
    /// seconds by default.
    #[serde(with = "humantime_serde", default = "default_clamav_connect_timeout")]
    pub connect_timeout: Duration,
    /// Inline scans hold a clamd connection for as long as the uploader
    /// takes; this caps how many at once. Beyond it the scan is deferred.
    /// 4 by default.
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

/// The derived representations of blobs: thumbnails, previews and the
/// tools that make them.
#[derive(Debug, Clone, Deserialize)]
pub struct MediaConfig {
    /// The version of every derived representation; bumping it regenerates
    /// them all on next request. At least 1, since zero means "none"
    /// inside sealed URLs; 1 by default.
    #[serde(default = "default_variant_version")]
    pub variant_version: i32,
    /// Originals above this size get no representations, except the text
    /// thumbnail, which reads only the first few KiB. 64 MiB by default.
    #[serde(default = "default_max_source_size")]
    pub max_source_size: ByteSize,
    /// Where the workers unpack originals while rendering; the system
    /// temporary directory by default.
    #[serde(default)]
    pub tmp_dir: Option<PathBuf>,
    /// The tools, boxes and sandbox limits, owned by the render crate and
    /// read from the same `[media]` table: `vips_command`,
    /// `extract_command`, `render_text_command`,
    /// `render_document_command`, `render_geo_command`, `thumbnail`,
    /// `preview`, `embedded_image`, `geojson`, `sandbox`.
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

/// The job queue and the workers this process runs.
#[derive(Debug, Clone, Deserialize)]
pub struct JobsConfig {
    /// How often idle workers look for work when no `NOTIFY` woke them;
    /// two seconds by default.
    #[serde(with = "humantime_serde", default = "default_poll_interval")]
    pub poll_interval: Duration,
    /// A job locked for longer than this is assumed orphaned and claimed
    /// again; running workers renew their lease. Ten minutes by default.
    #[serde(with = "humantime_serde", default = "default_lease_timeout")]
    pub lease_timeout: Duration,
    /// How often the reaper runs; a minute by default.
    #[serde(with = "humantime_serde", default = "default_reap_interval")]
    pub reap_interval: Duration,
    /// How long a dead job stays in the table before the reaper prunes it;
    /// seven days by default.
    #[serde(with = "humantime_serde", default = "default_keep_failed")]
    pub keep_failed: Duration,
    /// How often the audit sweeps the bucket against the `blobs` table;
    /// seven days by default.
    #[serde(with = "humantime_serde", default = "default_audit_interval")]
    pub audit_interval: Duration,
    #[serde(default)]
    /// Workers per job kind, each kind its own pool; a kind left out gets
    /// its default (`concurrency_for`). Keyed by kind, so an unknown name is
    /// a configuration error rather than a silently ignored line.
    pub concurrency: HashMap<JobKind, usize>,
    /// The kinds this process runs workers for; every kind when omitted.
    /// A deployment must cover each kind with some process.
    pub kinds: Option<Vec<JobKind>>,
}

impl JobsConfig {
    /// Workers for `kind`: the configured count, else a default suited to
    /// what the kind waits on.
    pub(crate) fn concurrency_for(&self, kind: JobKind) -> usize {
        if let Some(n) = self.concurrency.get(&kind) {
            return *n;
        }
        match kind {
            JobKind::Scan | JobKind::Render | JobKind::Purge | JobKind::Notify => 4,
            JobKind::Fetch | JobKind::Rows | JobKind::Document => 2,
            JobKind::Archive | JobKind::Reap | JobKind::Audit => 1,
        }
    }

    /// The kinds this process handles, in the order of `JobKind::ALL`, each
    /// once.
    pub(crate) fn kinds(&self) -> Vec<JobKind> {
        match &self.kinds {
            None => JobKind::ALL.to_vec(),
            Some(kinds) => JobKind::ALL
                .iter()
                .copied()
                .filter(|k| kinds.contains(k))
                .collect(),
        }
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
fn default_audit_interval() -> Duration {
    Duration::from_secs(7 * 24 * 3600)
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            poll_interval: default_poll_interval(),
            lease_timeout: default_lease_timeout(),
            reap_interval: default_reap_interval(),
            keep_failed: default_keep_failed(),
            audit_interval: default_audit_interval(),
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
            assert_eq!(config.jobs.concurrency_for(JobKind::Fetch), 2);
            assert_eq!(config.fetch.read_timeout.as_secs(), 30);
            assert_eq!(config.fetch.max_redirects, 5);
            assert_eq!(config.fetch.max_retry_after.as_secs(), 15 * 60);
            assert!(!config.fetch.allow_private_networks);
            assert!(config.fetch.user_agent.starts_with("agrafe/"));
            assert!(config.webhook.url.is_none(), "no webhook by default");
            assert!(!config.webhook.wants(Event::Ready));
            assert_eq!(config.webhook.timeout.as_secs(), 10);
            assert_eq!(config.jobs.concurrency_for(JobKind::Notify), 4);
            assert_eq!(config.jobs.concurrency_for(JobKind::Audit), 1);
            assert_eq!(config.jobs.audit_interval.as_secs(), 7 * 24 * 3600);
            Ok(())
        });
    }

    #[test]
    fn the_webhook_needs_a_secret_of_the_standard_shape() {
        Jail::expect_with(|jail| {
            jail.set_env(
                "AGRAFE_WEBHOOK__URL",
                "https://backend.example/agrafe/events",
            );
            let err = load(&example()).unwrap_err().to_string();
            assert!(err.contains("webhook.secret is required"), "{err}");

            jail.set_env("AGRAFE_WEBHOOK__SECRET", "hunter2");
            let err = load(&example()).unwrap_err().to_string();
            assert!(err.contains("must be whsec_"), "{err}");

            jail.set_env(
                "AGRAFE_WEBHOOK__SECRET",
                "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw",
            );
            let config = load(&example())?;
            assert!(config.webhook.wants(Event::Ready));
            assert_eq!(config.webhook.key().unwrap().len(), 24);

            jail.set_env("AGRAFE_WEBHOOK__EVENTS", r#"["infected"]"#);
            let config = load(&example())?;
            assert!(config.webhook.wants(Event::Infected));
            assert!(!config.webhook.wants(Event::Ready));

            jail.set_env("AGRAFE_WEBHOOK__URL", "ftp://backend.example/events");
            let err = load(&example()).unwrap_err().to_string();
            assert!(err.contains("http or https"), "{err}");

            jail.set_env("AGRAFE_WEBHOOK__URL", "http://backend.example/events");
            let err = load(&example()).unwrap_err().to_string();
            assert!(err.contains("must be https"), "{err}");
            jail.set_env("AGRAFE_WEBHOOK__ALLOW_INSECURE", "true");
            assert!(load(&example())?.webhook.wants(Event::Infected));
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
            assert_eq!(config.media.render.office.command, None);
            assert_eq!(config.media.render.office.sandbox.timeout.as_secs(), 120);

            jail.set_env("AGRAFE_MEDIA__VIPS_COMMAND", "/opt/vips/bin/vips");
            jail.set_env("AGRAFE_MEDIA__SANDBOX__TIMEOUT", "5s");
            jail.set_env("AGRAFE_MEDIA__MAX_SOURCE_SIZE", "1 MiB");
            jail.set_env("AGRAFE_MEDIA__OFFICE__COMMAND", "soffice");
            jail.set_env("AGRAFE_MEDIA__OFFICE__SANDBOX__MEMORY", "3 GiB");
            let config = load(&example())?;
            assert_eq!(config.media.render.vips_command, "/opt/vips/bin/vips");
            assert_eq!(config.media.render.sandbox.timeout.as_secs(), 5);
            assert_eq!(config.media.max_source_size.as_u64(), 1 << 20);
            assert_eq!(
                config.media.render.office.command.as_deref(),
                Some("soffice")
            );
            assert_eq!(
                config.media.render.office.sandbox.memory,
                bytesize::ByteSize::gib(3)
            );
            Ok(())
        });
    }

    #[test]
    fn keks_file_is_a_path_the_environment_can_set() {
        Jail::expect_with(|jail| {
            let config = load(&example())?;
            assert_eq!(config.encryption.keks_file, None);
            jail.set_env(
                "AGRAFE_ENCRYPTION__KEKS_FILE",
                "/run/credentials/agrafe.service/keks",
            );
            let config = load(&example())?;
            assert_eq!(
                config.encryption.keks_file.as_deref(),
                Some(Path::new("/run/credentials/agrafe.service/keks"))
            );
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
        assert!(cfg.kinds().contains(&JobKind::Archive));

        let cfg = jobs(r#"kinds = ["archive", "scan", "scan"]"#).unwrap();
        assert_eq!(cfg.kinds(), [JobKind::Scan, JobKind::Archive]);
        assert!(!cfg.kinds().contains(&JobKind::Render));
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
            assert_eq!(config.upload.abandoned_after.as_secs(), 24 * 3600);
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
