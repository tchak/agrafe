//! The fetch job: download the bytes of a `url` blob and run them through
//! the upload pipeline, as a `PUT` would. See [DESIGN.md, "Fetched blobs"].
//!
//! The job is an uploader. It claims the row `created → uploading` the way
//! the `PUT` handler does, hands the response body to the same pipeline
//! (size, hashes, sniffing, inline scan, encryption, multipart store) and
//! lands on `uploaded` or `ready`. Write-once, the heartbeat, the stale
//! reset and the hand-back to `created` on failure all apply unchanged.
//!
//! The URL is a presigned secret: it is read off the row and never written
//! into an error, a log or a span. reqwest's errors carry the URL, so
//! every one of them goes through `without_url` before it is stringified.
//!
//! [DESIGN.md, "Fetched blobs"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#fetched-blobs

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use jiff::Timestamp;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{StatusCode, redirect};
use url::{Host, Url};

use super::{Job, JobError};
use crate::app::Agrafe;
use crate::blob::repo::Transition;
use crate::blob::{Blob, BlobState, InputType};
use crate::config::FetchConfig;
use crate::upload::{self, BoxError, Declared, UploadError};

/// Longest URL accepted at creation, in bytes. A presigned URL is a
/// kilobyte or two; nothing legitimate needs more than this.
pub const MAX_URL_LEN: usize = 8 * 1024;

/// Why a URL may not be fetched: the network policy of [DESIGN.md,
/// "Fetched blobs"]. Checked at creation, on the first request and on
/// every redirect.
///
/// [DESIGN.md, "Fetched blobs"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#fetched-blobs
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("url does not parse")]
    Invalid,
    #[error("url is too long")]
    TooLong,
    #[error("url must use https")]
    Scheme,
    #[error("url must not carry credentials")]
    Credentials,
    #[error("url has no host")]
    NoHost,
    /// A loopback, private, link-local or otherwise refused address, as a
    /// literal in the URL or behind its host name.
    #[error("host {0} is on a refused network")]
    Refused(String),
    #[error("more than {0} redirects")]
    TooManyRedirects(usize),
}

/// Parse `raw` and check it against the policy, host literals included.
/// A host name is resolved and checked when the fetch runs.
pub fn check_url(raw: &str, config: &FetchConfig) -> Result<Url, PolicyError> {
    let raw = raw.trim();
    if raw.len() > MAX_URL_LEN {
        return Err(PolicyError::TooLong);
    }
    let url = Url::parse(raw).map_err(|_| PolicyError::Invalid)?;
    check(&url, config.allow_private_networks)?;
    Ok(url)
}

/// The checks that need no network: the scheme, the absence of
/// credentials, and the address when the host is one.
fn check(url: &Url, allow_private: bool) -> Result<(), PolicyError> {
    match url.scheme() {
        "https" => {}
        "http" if allow_private => {}
        _ => return Err(PolicyError::Scheme),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(PolicyError::Credentials);
    }
    let ip = match url.host() {
        None => return Err(PolicyError::NoHost),
        Some(Host::Domain(_)) => return Ok(()),
        Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
    };
    if !allow_private && refused(ip) {
        return Err(PolicyError::Refused(ip.to_string()));
    }
    Ok(())
}

/// Whether an address is one the service must not connect to: loopback,
/// private, link-local (where cloud metadata lives), shared address space,
/// unspecified, multicast, broadcast, documentation and reserved ranges,
/// and their IPv6 forms, including an IPv4 address mapped into IPv6.
pub fn refused(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                // 0.0.0.0/8, "this network".
                || o[0] == 0
                // 100.64.0.0/10, carrier-grade NAT.
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                // 192.0.0.0/24, IETF protocol assignments.
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                // 198.18.0.0/15, benchmarking.
                || (o[0] == 198 && (o[1] & 0xfe) == 18)
                // 240.0.0.0/4, reserved.
                || o[0] >= 240
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return refused(IpAddr::V4(v4));
            }
            let s = v6.segments();
            v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                // 2001:db8::/32, documentation.
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                // 64:ff9b::/96, NAT64: the IPv4 address is in the low bits.
                || (s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0; 4]
                    && refused(IpAddr::V4(std::net::Ipv4Addr::new(
                        (s[6] >> 8) as u8,
                        s[6] as u8,
                        (s[7] >> 8) as u8,
                        s[7] as u8,
                    ))))
                // ::ffff:0:0/96 handled above; ::/96 compatibility addresses.
                || (s[..6] == [0; 6] && (s[6] != 0 || s[7] != 1))
        }
    }
}

/// The resolver the client connects through: the system's, with every
/// address checked against the policy. The connection is then made to
/// the addresses returned here, so a name cannot be rebound to another
/// address between the check and the connect. A host literal never
/// reaches a resolver; `check` covers it.
struct Resolver {
    allow_private: bool,
}

/// Why a name gave no usable address. Found in the chain of the request
/// error it causes, to tell a refused host, permanent, from a name that
/// did not resolve, which is as often the resolver's bad minute as a typo
/// and is retried.
#[derive(Debug, thiserror::Error)]
enum ResolveError {
    #[error("host {0} does not resolve: {1}")]
    Unresolved(String, std::io::Error),
    #[error("host {0} does not resolve")]
    NoAddress(String),
    #[error("host {0} resolves to a refused address")]
    Refused(String),
}

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private = self.allow_private;
        Box::pin(async move {
            let host = name.as_str().to_string();
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| ResolveError::Unresolved(host.clone(), e))?
                .collect();
            if addrs.is_empty() {
                return Err(ResolveError::NoAddress(host).into());
            }
            // One refused address among several refuses the name: a name
            // that answers with a public and a private address is a trick.
            if !allow_private && addrs.iter().any(|a| refused(a.ip())) {
                return Err(ResolveError::Refused(host).into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// The HTTP client fetches run through: the network policy, the
/// timeouts (connect, a stalled read, and the whole response as a
/// backstop), a fixed `User-Agent`, no cookies, no proxy, redirects
/// bounded and checked hop by hop. One per process, built from `[fetch]`.
#[derive(Clone)]
pub struct Client {
    inner: reqwest::Client,
    max_retry_after: Duration,
    /// How long the remote may stay silent, for its head or the next
    /// bytes of its body. Timed here, not by reqwest, whose read timeout
    /// starts when the bytes are asked for: a pipeline that stops reading
    /// for a while, a store write or a scan behind, would count against
    /// the remote.
    read_timeout: Duration,
}

impl Client {
    pub fn from_config(config: &FetchConfig) -> Result<Self, reqwest::Error> {
        let allow_private = config.allow_private_networks;
        let max_redirects = config.max_redirects;
        let policy = redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > max_redirects {
                return attempt.error(PolicyError::TooManyRedirects(max_redirects));
            }
            match check(attempt.url(), allow_private) {
                Ok(()) => attempt.follow(),
                Err(err) => attempt.error(err),
            }
        });
        let inner = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .connect_timeout(config.connect_timeout)
            .timeout(config.timeout)
            .redirect(policy)
            .referer(false)
            .no_proxy()
            .https_only(!allow_private)
            .dns_resolver(Arc::new(Resolver { allow_private }))
            .build()?;
        Ok(Self {
            inner,
            max_retry_after: config.max_retry_after,
            read_timeout: config.read_timeout,
        })
    }

    /// `GET` the URL, following redirects under the policy, and sort the
    /// status into what the job does next.
    async fn get(&self, url: Url) -> Result<reqwest::Response, FetchError> {
        let response = tokio::time::timeout(self.read_timeout, self.inner.get(url).send())
            .await
            .map_err(|_| FetchError::Stalled(self.read_timeout))?
            .map_err(classify)?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        if status.is_server_error() || matches!(status.as_u16(), 408 | 425 | 429) {
            let after = retry_after(response.headers(), Timestamp::now());
            if let Some(after) = after
                && after > self.max_retry_after
            {
                return Err(FetchError::RetryAfterTooLong { status, after });
            }
            return Err(FetchError::Unavailable { status, after });
        }
        Err(FetchError::Status(status))
    }

    /// The response's body, each wait for the next bytes bounded by
    /// `read_timeout` from the moment the pipeline asks for them, so that
    /// only the remote's silence counts.
    fn body(&self, response: reqwest::Response) -> BoxStream<'static, Result<Bytes, BoxError>> {
        let read_timeout = self.read_timeout;
        stream::unfold(
            Some(response.bytes_stream().boxed()),
            move |body| async move {
                let mut body = body?;
                match tokio::time::timeout(read_timeout, body.next()).await {
                    Ok(Some(chunk)) => Some((
                        chunk.map_err(|e| BoxError::from(e.without_url())),
                        Some(body),
                    )),
                    Ok(None) => None,
                    // The stream ends after the error.
                    Err(_) => Some((Err(FetchError::Stalled(read_timeout).into()), None)),
                }
            },
        )
        .boxed()
    }
}

/// Why one attempt at a fetch failed. `is_permanent` says which kill the
/// job at once and which go through the backoff.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("blob has no url")]
    NoUrl,
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// The row is `uploading`: this fetch, or an attempt before it that
    /// was cut short and that the reaper has not reset yet. Not a quiet
    /// success: a job that gave up here would leave the blob with no job.
    /// `retry_in` is when the reaper will have reset the row, so a single
    /// attempt covers the wait instead of the backoff eating the budget.
    #[error("blob is uploading: a fetch is in flight or was cut short")]
    Uploading { retry_in: Duration },
    /// A status another attempt would get again: a 4xx other than 408,
    /// 425 and 429.
    #[error("the remote answered {0}")]
    Status(StatusCode),
    /// A 5xx, 408, 425 or 429: retried, no sooner than `Retry-After`.
    #[error("the remote answered {status}")]
    Unavailable {
        status: StatusCode,
        after: Option<Duration>,
    },
    #[error(
        "the remote answered {status} asking to retry after {after:?}, beyond fetch.max_retry_after"
    )]
    RetryAfterTooLong { status: StatusCode, after: Duration },
    #[error("Content-Length {len} does not match the declared size {declared}")]
    ContentLength { len: u64, declared: u64 },
    #[error("Content-Length {len} exceeds the maximum of {max} bytes")]
    TooLarge { len: u64, max: u64 },
    /// The host resolves to a refused address.
    #[error("{0}")]
    Refused(String),
    /// The host did not resolve, or had no address: as likely the
    /// resolver's bad minute as a typo, so retried.
    #[error("{0}")]
    Unresolved(String),
    #[error("tls: {0}")]
    Tls(String),
    /// The remote sent nothing for `fetch.read_timeout`, its head or the
    /// next bytes of its body: the remote, never the pipeline behind.
    #[error("the remote sent nothing for {0:?}")]
    Stalled(Duration),
    /// A connect timeout, a reset, a body cut short: the network.
    #[error("request: {0}")]
    Request(String),
    #[error("{0}")]
    Upload(UploadError),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

impl FetchError {
    /// Whether the next attempt would fail the same way, see [DESIGN.md,
    /// "Fetched blobs"].
    ///
    /// [DESIGN.md, "Fetched blobs"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#fetched-blobs
    pub fn is_permanent(&self) -> bool {
        match self {
            FetchError::NoUrl
            | FetchError::Policy(_)
            | FetchError::Status(_)
            | FetchError::RetryAfterTooLong { .. }
            | FetchError::ContentLength { .. }
            | FetchError::TooLarge { .. }
            | FetchError::Refused(_)
            | FetchError::Tls(_) => true,
            FetchError::Uploading { .. }
            | FetchError::Unresolved(_)
            | FetchError::Unavailable { .. }
            | FetchError::Stalled(_)
            | FetchError::Request(_)
            | FetchError::Db(_) => false,
            FetchError::Upload(err) => !matches!(
                err,
                // The body, the bucket, the database, or a row that
                // changed under the fetch: the next attempt reads the
                // world afresh.
                UploadError::Body(_)
                    | UploadError::Store(_)
                    | UploadError::Crypto(_)
                    | UploadError::Db(_)
                    | UploadError::Task(_)
                    | UploadError::Conflict(_)
                    | UploadError::NotFound
            ),
        }
    }

    /// What to wait before the next attempt, when the failure knows: the
    /// remote's `Retry-After`, or the stale reset of a row left `uploading`.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            FetchError::Unavailable { after, .. } => *after,
            FetchError::Uploading { retry_in } => Some(*retry_in),
            _ => None,
        }
    }
}

/// Sort a request error: the policy's own refusals, found in the error's
/// chain, and a TLS failure are permanent; a name that did not resolve and
/// everything else is the network and is retried.
fn classify(err: reqwest::Error) -> FetchError {
    if let Some(policy) = find_in_chain::<PolicyError>(&err) {
        return FetchError::Policy(policy.clone());
    }
    if let Some(resolve) = find_in_chain::<ResolveError>(&err) {
        return match resolve {
            ResolveError::Refused(_) => FetchError::Refused(resolve.to_string()),
            ResolveError::Unresolved(..) | ResolveError::NoAddress(_) => {
                FetchError::Unresolved(resolve.to_string())
            }
        };
    }
    let tls = find_in_chain::<rustls::Error>(&err).is_some();
    let message = err.without_url().to_string();
    if tls {
        FetchError::Tls(message)
    } else {
        FetchError::Request(message)
    }
}

/// The first `T` in the chain of causes of `err`, itself included. An
/// `io::Error` reports its own inner error's cause as its `source`, not
/// the inner error itself, which is where a TLS failure sits: it is looked
/// at directly.
fn find_in_chain<'a, T: std::error::Error + 'static>(
    err: &'a (dyn std::error::Error + 'static),
) -> Option<&'a T> {
    let mut current = Some(err);
    while let Some(e) = current {
        if let Some(found) = e.downcast_ref::<T>() {
            return Some(found);
        }
        current = match e
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref())
        {
            Some(inner) => Some(inner as &(dyn std::error::Error + 'static)),
            None => e.source(),
        };
    }
    None
}

/// `Retry-After` as a delay from `now`: delta seconds, or an HTTP date.
/// A header that does not parse is ignored.
fn retry_after(headers: &HeaderMap, now: Timestamp) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = jiff::fmt::rfc2822::DateTimeParser::new()
        .parse_timestamp(value)
        .ok()?;
    Some(at.duration_since(now).try_into().unwrap_or(Duration::ZERO))
}

pub async fn run(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    let id = job.blob_id.ok_or(JobError::NoBlob(job.kind))?;
    let Some(blob) = crate::blob::repo::find(state.pool(), id).await? else {
        tracing::info!("blob is gone, nothing to fetch");
        return Ok(());
    };
    if blob.input_type != InputType::Url {
        tracing::info!(input_type = %blob.input_type, "blob is not fetched");
        return Ok(());
    }
    // By the state at this instant: a blob past its expiry is expired
    // whether or not the reaper has recorded it, and its bytes would be
    // fetched for nothing.
    match blob.state_at(Timestamp::now()) {
        BlobState::Created => {}
        BlobState::Uploading => return Err(uploading(state, &blob).into()),
        other => {
            tracing::info!(state = %other, "blob is not waiting for a fetch");
            return Ok(());
        }
    }
    fetch(state, blob).await?;
    Ok(())
}

/// The failure for a row found `uploading`, due again once the reaper has
/// had the time to reset it: `upload.stale_after` past its last heartbeat,
/// plus one `jobs.reap_interval` for the reaper to come round. The
/// backoff applies when that is sooner.
fn uploading(state: &Agrafe, blob: &Blob) -> FetchError {
    let config = state.config();
    let heartbeat = blob
        .upload_heartbeat_at
        .or(blob.upload_started_at)
        .unwrap_or_else(Timestamp::now);
    let reset_by = heartbeat
        .checked_add(config.upload.stale_after)
        .and_then(|at| at.checked_add(config.jobs.reap_interval))
        .unwrap_or(heartbeat);
    let retry_in = reset_by
        .duration_since(Timestamp::now())
        .try_into()
        .unwrap_or(Duration::ZERO);
    FetchError::Uploading { retry_in }
}

/// Open the URL, claim the row, and feed the body to the pipeline.
async fn fetch(state: &Agrafe, blob: Blob) -> Result<(), FetchError> {
    let config = state.config();
    let raw = blob.url.as_deref().ok_or(FetchError::NoUrl)?;
    let url = check_url(raw, &config.fetch)?;
    let response = state.fetcher().get(url).await?;

    let mut declared = Declared::fetched(&blob, &config.upload);
    if let Some(len) = response.content_length() {
        match declared.size {
            Some(size) if size != len => {
                return Err(FetchError::ContentLength {
                    len,
                    declared: size,
                });
            }
            Some(_) => {}
            None if len > declared.cap => {
                return Err(FetchError::TooLarge {
                    len,
                    max: declared.cap,
                });
            }
            // The remote's word bounds the body and settles the inline
            // scan decision, as a declaration would; hyper fails the body
            // if it falls short of it. It is not a declaration: nothing
            // the backend did not say is held against the bytes.
            None => declared.cap = len,
        }
    }

    // The claim comes once the remote has answered, so the row is
    // `uploading` for the body alone, not for a slow connect.
    let blob = match upload::begin(state, &blob).await? {
        Transition::Done(blob) => blob,
        Transition::Conflict(current) if current.state == BlobState::Uploading => {
            return Err(uploading(state, &current));
        }
        Transition::Conflict(current) => {
            tracing::info!(state = %current.state, "blob changed state before the fetch began");
            return Ok(());
        }
        Transition::Missing => {
            tracing::info!("blob is gone, nothing to fetch");
            return Ok(());
        }
    };
    let body = state.fetcher().body(response);
    match upload::store_and_complete(state.clone(), blob, declared, body).await {
        Ok(blob) => {
            tracing::info!(state = %blob.state, size = blob.size, "fetched");
            Ok(())
        }
        // The bytes are stored and the blob is `infected`: the fetch did
        // its job, and the verdict is on the row.
        Err(UploadError::Infected(signature)) => {
            tracing::warn!(signature, "fetched an infected file");
            Ok(())
        }
        Err(err) => Err(FetchError::Upload(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A remote on loopback that answers the head at once and the body in
    /// two halves, `pause` apart.
    async fn remote(pause: Duration) -> Url {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhello")
                .await
                .unwrap();
            tokio::time::sleep(pause).await;
            let _ = sock.write_all(b"world").await;
        });
        format!("http://{addr}/file").parse().unwrap()
    }

    fn quick_client() -> Client {
        Client::from_config(&FetchConfig {
            allow_private_networks: true,
            read_timeout: Duration::from_millis(500),
            ..FetchConfig::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn a_pipeline_that_stops_reading_is_not_the_remote_stalling() {
        let client = quick_client();
        let response = client
            .get(remote(Duration::from_millis(50)).await)
            .await
            .unwrap();
        let mut body = client.body(response);
        let mut got = Vec::new();
        while let Some(chunk) = body.next().await {
            got.extend_from_slice(&chunk.unwrap());
            // Longer than the read timeout, between reads: a store write.
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert_eq!(got, b"helloworld");
    }

    #[tokio::test]
    async fn a_remote_that_goes_silent_stalls() {
        let client = quick_client();
        let response = client
            .get(remote(Duration::from_secs(5)).await)
            .await
            .unwrap();
        let mut body = client.body(response);
        assert_eq!(&body.next().await.unwrap().unwrap()[..], b"hello");
        let err = body.next().await.unwrap().unwrap_err();
        assert_eq!(err.to_string(), "the remote sent nothing for 500ms");
        assert!(body.next().await.is_none());
    }

    fn config(allow_private: bool) -> FetchConfig {
        FetchConfig {
            allow_private_networks: allow_private,
            ..FetchConfig::default()
        }
    }

    #[test]
    fn refused_ranges() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "0.1.2.3",
            "224.0.0.1",
            "255.255.255.255",
            "192.0.2.1",
            "198.18.0.1",
            "240.0.0.1",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
            "2001:db8::1",
        ] {
            assert!(refused(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(!refused(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn url_policy() {
        let strict = config(false);
        assert!(check_url("https://files.example/a?b=c", &strict).is_ok());
        assert_eq!(
            check_url("http://files.example/a", &strict),
            Err(PolicyError::Scheme)
        );
        assert_eq!(
            check_url("ftp://files.example/a", &strict),
            Err(PolicyError::Scheme)
        );
        assert_eq!(
            check_url("https://user:pw@files.example/a", &strict),
            Err(PolicyError::Credentials)
        );
        assert_eq!(
            check_url("https://user@files.example/a", &strict),
            Err(PolicyError::Credentials)
        );
        assert_eq!(check_url("not a url", &strict), Err(PolicyError::Invalid));
        assert_eq!(
            check_url("https://127.0.0.1/a", &strict),
            Err(PolicyError::Refused("127.0.0.1".into()))
        );
        assert_eq!(
            check_url("https://[::1]/a", &strict),
            Err(PolicyError::Refused("::1".into()))
        );
        assert_eq!(
            check_url("https://169.254.169.254/latest/meta-data", &strict),
            Err(PolicyError::Refused("169.254.169.254".into()))
        );
        let long = format!("https://files.example/{}", "a".repeat(MAX_URL_LEN));
        assert_eq!(check_url(&long, &strict), Err(PolicyError::TooLong));
        let padded = format!("https://files.example/{}", "a".repeat(MAX_URL_LEN - 22));
        assert_eq!(padded.len(), MAX_URL_LEN);
        assert!(
            check_url(&format!("  {padded}  "), &strict).is_ok(),
            "measured trimmed"
        );

        // Host names are checked when they resolve, not here.
        assert!(check_url("https://localhost/a", &strict).is_ok());

        let lax = config(true);
        assert!(check_url("http://127.0.0.1:8080/a", &lax).is_ok());
        assert!(check_url("https://10.0.0.1/a", &lax).is_ok());
        assert_eq!(
            check_url("http://user:pw@127.0.0.1/a", &lax),
            Err(PolicyError::Credentials)
        );
    }

    #[test]
    fn retry_after_header() {
        let now = Timestamp::now();
        let mut headers = HeaderMap::new();
        assert_eq!(retry_after(&headers, now), None);
        headers.insert(RETRY_AFTER, "120".parse().unwrap());
        assert_eq!(retry_after(&headers, now), Some(Duration::from_secs(120)));
        headers.insert(RETRY_AFTER, "soon".parse().unwrap());
        assert_eq!(retry_after(&headers, now), None);
        let http_date = |at: Timestamp| {
            jiff::fmt::rfc2822::to_string(&at.to_zoned(jiff::tz::TimeZone::UTC)).unwrap()
        };
        let date = http_date(now + Duration::from_secs(300));
        headers.insert(RETRY_AFTER, date.parse().unwrap());
        let got = retry_after(&headers, now).unwrap();
        assert!((299..=300).contains(&got.as_secs()), "{got:?}");
        let past = http_date(now - Duration::from_secs(60));
        headers.insert(RETRY_AFTER, past.parse().unwrap());
        assert_eq!(retry_after(&headers, now), Some(Duration::ZERO));
    }

    #[test]
    fn permanence() {
        assert!(FetchError::Status(StatusCode::NOT_FOUND).is_permanent());
        assert!(FetchError::Policy(PolicyError::Scheme).is_permanent());
        assert!(FetchError::Refused("x".into()).is_permanent());
        assert!(!FetchError::Unresolved("x".into()).is_permanent());
        assert!(FetchError::Tls("x".into()).is_permanent());
        assert!(
            FetchError::ContentLength {
                len: 1,
                declared: 2
            }
            .is_permanent()
        );
        assert!(FetchError::Upload(UploadError::ChecksumMismatch).is_permanent());
        assert!(FetchError::Upload(UploadError::Md5Mismatch).is_permanent());
        assert!(FetchError::Upload(UploadError::SizeMismatch).is_permanent());
        assert!(FetchError::Upload(UploadError::BodyTooLarge(1)).is_permanent());
        let uploading = FetchError::Uploading {
            retry_in: Duration::from_secs(660),
        };
        assert!(!uploading.is_permanent());
        assert_eq!(uploading.retry_after(), Some(Duration::from_secs(660)));
        assert!(!FetchError::Request("reset".into()).is_permanent());
        assert!(
            !FetchError::Unavailable {
                status: StatusCode::SERVICE_UNAVAILABLE,
                after: None
            }
            .is_permanent()
        );
        assert!(
            FetchError::RetryAfterTooLong {
                status: StatusCode::TOO_MANY_REQUESTS,
                after: Duration::from_secs(3600)
            }
            .is_permanent()
        );
        assert!(!FetchError::Upload(UploadError::Body("cut".into())).is_permanent());
        assert!(!FetchError::Upload(UploadError::Conflict(BlobState::Created)).is_permanent());
        let after = FetchError::Unavailable {
            status: StatusCode::TOO_MANY_REQUESTS,
            after: Some(Duration::from_secs(7)),
        };
        assert_eq!(after.retry_after(), Some(Duration::from_secs(7)));
    }

    #[test]
    fn errors_in_the_chain_are_found() {
        let inner: BoxError = Box::new(PolicyError::Credentials);
        let outer = std::io::Error::other(inner);
        assert_eq!(
            find_in_chain::<PolicyError>(&outer),
            Some(&PolicyError::Credentials)
        );
        assert!(find_in_chain::<ResolveError>(&outer).is_none());
    }
}
