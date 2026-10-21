//! A `clamd` client speaking `INSTREAM` over TCP.
//!
//! One session per file: connect, announce the command, send the bytes as
//! length-prefixed chunks, send an empty chunk, read one line back. clamd
//! scans only once the stream is complete, but it hangs up before that
//! when its stream limit is hit (after an `ERROR` reply) or when it dies,
//! so a failed write is not final: `finish` still reads whatever reply is
//! pending and reports the write failure only if there is none.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::ClamavConfig;

/// Largest `INSTREAM` chunk sent at once.
const CHUNK: usize = 64 * 1024;

/// Longest reply we are willing to buffer.
const MAX_REPLY: usize = 4096;

#[derive(Debug, Clone)]
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub struct Clamd {
    addr: String,
    timeout: Duration,
    connect_timeout: Duration,
    /// Permits for inline sessions, which last as long as the upload they
    /// ride on. Without a cap, slow uploaders would hold every clamd thread.
    inline: Arc<Semaphore>,
    max_inline: usize,
}

/// What clamd said about the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub enum Verdict {
    Clean,
    /// The signature name, such as `Eicar-Test-Signature`.
    Infected(String),
}

/// A scan that gave no verdict. Each is retried by the scan job: none
/// says anything about the bytes.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Connecting to clamd, writing to it or reading its reply failed: it
    /// is down, or it hung up.
    #[error("clamd at {addr}: {source}")]
    Io {
        /// `clamav.addr`.
        addr: String,
        /// The socket error.
        #[source]
        source: std::io::Error,
    },
    /// The connect, a write or the wait for the verdict outlasted its
    /// timeout, `clamav.connect_timeout` or `clamav.timeout`.
    #[error("clamd at {addr}: no answer within {timeout:?}")]
    Timeout {
        /// `clamav.addr`.
        addr: String,
        /// The timeout that ran out.
        timeout: Duration,
    },
    /// clamd answered with neither `OK` nor `FOUND`, such as an `ERROR`
    /// when the stream exceeds its `StreamMaxLength`.
    #[error("clamd at {addr} replied `{reply}`")]
    Reply {
        /// `clamav.addr`.
        addr: String,
        /// The reply, trimmed.
        reply: String,
    },
    /// Every one of the `clamav.max_inline_scans` inline sessions is in
    /// use; the upload goes on and its scan is deferred to a job.
    #[error("clamd at {addr}: all {limit} inline sessions are taken")]
    Busy {
        /// `clamav.addr`.
        addr: String,
        /// `clamav.max_inline_scans`.
        limit: usize,
    },
}

impl Clamd {
    pub(crate) fn from_config(config: &ClamavConfig) -> Self {
        Self {
            addr: config.addr.clone(),
            timeout: config.timeout,
            connect_timeout: config.connect_timeout,
            inline: Arc::new(Semaphore::new(config.max_inline_scans)),
            max_inline: config.max_inline_scans,
        }
    }

    /// Connect and open an `INSTREAM` session for a scan whose pace this
    /// process controls: a worker streaming a stored object.
    pub async fn begin(&self) -> Result<Session, Error> {
        self.connect(None).await
    }

    /// Open a session that will be fed by an upload in flight, if one of the
    /// `max_inline_scans` slots is free. `Busy` otherwise, at once: the
    /// upload is not held up, its scan is deferred.
    pub async fn begin_inline(&self) -> Result<Session, Error> {
        let permit = self
            .inline
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Busy {
                addr: self.addr.clone(),
                limit: self.max_inline,
            })?;
        self.connect(Some(permit)).await
    }

    #[tracing::instrument(name = "clamav", level = "info", skip_all, fields(op = "connect"))]
    async fn connect(&self, permit: Option<OwnedSemaphorePermit>) -> Result<Session, Error> {
        let mut stream = self
            .wait_for(self.connect_timeout, TcpStream::connect(&self.addr))
            .await?
            .map_err(|e| self.io(e))?;
        self.wait(stream.write_all(b"zINSTREAM\0"))
            .await?
            .map_err(|e| self.io(e))?;
        Ok(Session {
            clamd: self.clone(),
            stream,
            broken: None,
            _permit: permit,
        })
    }

    async fn wait<T>(&self, fut: impl Future<Output = T>) -> Result<T, Error> {
        self.wait_for(self.timeout, fut).await
    }

    async fn wait_for<T>(
        &self,
        timeout: Duration,
        fut: impl Future<Output = T>,
    ) -> Result<T, Error> {
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| Error::Timeout {
                addr: self.addr.clone(),
                timeout,
            })
    }

    fn io(&self, source: std::io::Error) -> Error {
        Error::Io {
            addr: self.addr.clone(),
            source,
        }
    }
}

/// An open `INSTREAM` session. Feed it the plaintext, then `finish`.
// Public for the integration tests, not part of the API.
#[doc(hidden)]
pub struct Session {
    clamd: Clamd,
    stream: TcpStream,
    /// The first write that failed. clamd hangs up early at its stream
    /// limit, with an `ERROR` that may still be readable.
    broken: Option<Error>,
    /// Held for the life of an inline session, released with it.
    _permit: Option<OwnedSemaphorePermit>,
}

impl Session {
    /// Send more bytes. Returns whether clamd is still taking them: once a
    /// write has failed it is not, further bytes are dropped, and `finish`
    /// reports the verdict or the error waiting on the socket. A caller
    /// producing the bytes for clamd alone should stop at `false`.
    pub async fn write(&mut self, bytes: &[u8]) -> bool {
        if self.broken.is_some() {
            return false;
        }
        for chunk in bytes.chunks(CHUNK) {
            let len = (chunk.len() as u32).to_be_bytes();
            let result = async {
                self.stream.write_all(&len).await?;
                self.stream.write_all(chunk).await
            };
            if let Err(err) = self
                .clamd
                .wait(result)
                .await
                .and_then(|r| r.map_err(|e| self.clamd.io(e)))
            {
                self.broken = Some(err);
                return false;
            }
        }
        true
    }

    /// Close the stream and read the verdict. Traced: the wait for the
    /// verdict is clamd's scan of whatever it has not scanned yet.
    #[tracing::instrument(name = "clamav", level = "info", skip_all, fields(op = "verdict"))]
    pub async fn finish(mut self) -> Result<Verdict, Error> {
        if self.broken.is_none() {
            let end = 0u32.to_be_bytes();
            let result = self.stream.write_all(&end);
            if let Err(err) = self
                .clamd
                .wait(result)
                .await
                .and_then(|r| r.map_err(|e| self.clamd.io(e)))
            {
                self.broken = Some(err);
            }
        }
        let _ = self.stream.shutdown().await;
        let reply = match self.clamd.wait(read_reply(&mut self.stream)).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(err)) => return Err(self.broken.unwrap_or_else(|| self.clamd.io(err))),
            Err(timeout) => return Err(self.broken.unwrap_or(timeout)),
        };
        match parse_reply(&reply) {
            Some(verdict) => Ok(verdict),
            None => Err(self.broken.unwrap_or(Error::Reply {
                addr: self.clamd.addr.clone(),
                reply,
            })),
        }
    }
}

/// Read until the NUL terminator or the end of the stream.
async fn read_reply(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte).await? {
            0 => break,
            _ if byte[0] == 0 => break,
            _ => buf.push(byte[0]),
        }
        if buf.len() > MAX_REPLY {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).trim().to_string())
}

/// `stream: OK`, `stream: Name FOUND`, or anything else (an `ERROR`).
fn parse_reply(reply: &str) -> Option<Verdict> {
    let body = reply
        .strip_prefix("stream:")
        .map(str::trim)
        .unwrap_or(reply);
    if body == "OK" {
        return Some(Verdict::Clean);
    }
    body.strip_suffix("FOUND")
        .map(|name| Verdict::Infected(name.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies() {
        assert_eq!(parse_reply("stream: OK"), Some(Verdict::Clean));
        assert_eq!(
            parse_reply("stream: Eicar-Test-Signature FOUND"),
            Some(Verdict::Infected("Eicar-Test-Signature".into()))
        );
        assert_eq!(parse_reply("INSTREAM size limit exceeded. ERROR"), None);
        assert_eq!(parse_reply(""), None);
    }
}
