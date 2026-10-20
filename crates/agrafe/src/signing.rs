//! Signed public URLs.
//!
//! Token (one path segment): `v1.<key_id>.<expires_unix>.<base64url(hmac)>`
//! Message: `METHOD "\n" expires_unix "\n" path_and_query_after_token`
//!
//! The tail is the raw request target starting at `/<id>`, including the query
//! string. Nothing is decoded or re-encoded on either side; the server mints
//! and the server verifies.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use http::Method;
use sha2::Sha256;

use crate::config::{ConfigError, SigningConfig};

type HmacSha256 = Hmac<Sha256>;

const VERSION: &str = "v1";

/// Why a token presented to the public port is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    /// The token is malformed, names an unknown key, or its MAC does not
    /// match the method and request target. Nothing else is checked first,
    /// so a forged token learns nothing more.
    #[error("invalid signature")]
    Invalid,
    /// The signature is good but its expiry has passed.
    #[error("signature expired")]
    Expired,
    /// Signed by a known key, but for longer than `signing.ttl.max` allows:
    /// the minter's mistake, refused so that the ceiling holds for every
    /// holder of the key.
    #[error("signature outlives signing.ttl.max")]
    TooLong,
}

/// Mints and verifies the HMAC tokens of public URLs, with the keys of
/// `[signing]`: the current one to mint, any of them to verify.
#[derive(Clone)]
pub struct Signer {
    keys: HashMap<String, Vec<u8>>,
    current: String,
    window: Duration,
    max_ttl: Duration,
}

impl Signer {
    /// Build from config, validating everything the signer depends on.
    pub(crate) fn from_config(config: &SigningConfig) -> Result<Self, ConfigError> {
        let invalid = ConfigError::Invalid;
        if config.window.is_zero() {
            return Err(invalid("signing.window must be positive".into()));
        }
        let ttl = &config.ttl;
        if ttl.max.is_zero() {
            return Err(invalid("signing.ttl.max must be positive".into()));
        }
        for (name, value) in [
            ("upload", ttl.upload),
            ("metadata", ttl.metadata),
            ("read", ttl.read),
        ] {
            if value > ttl.max {
                return Err(invalid(format!(
                    "signing.ttl.{name} ({value:?}) exceeds signing.ttl.max ({:?})",
                    ttl.max
                )));
            }
        }
        let mut keys = HashMap::new();
        for (id, secret) in &config.keys {
            // The id travels inside the URL token, so it must be a safe path
            // segment and must not contain the token separator `.`.
            if id.is_empty()
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(invalid(format!(
                    "signing key id `{id}` must be non-empty and use only [A-Za-z0-9_-]"
                )));
            }
            let key = secret
                .decoded()
                .map_err(|e| invalid(format!("signing key `{id}` is not valid base64: {e}")))?;
            if key.len() < 32 {
                return Err(invalid(format!(
                    "signing key `{id}` must be at least 32 bytes"
                )));
            }
            keys.insert(id.clone(), key);
        }
        if !keys.contains_key(&config.current_key) {
            return Err(invalid(format!(
                "signing.current_key `{}` is not present in signing.keys",
                config.current_key
            )));
        }
        Ok(Self {
            keys,
            current: config.current_key.clone(),
            window: config.window,
            max_ttl: config.ttl.max,
        })
    }

    /// The longest a URL may live, `signing.ttl.max`.
    pub fn max_ttl(&self) -> Duration {
        self.max_ttl
    }

    /// Expiry for a URL minted now: rounded up to the next window boundary,
    /// then the TTL is added. Every mint inside one window yields the same value.
    pub(crate) fn expiry_for(&self, ttl: Duration, now: SystemTime) -> u64 {
        let now = unix(now);
        let window = self.window.as_secs().max(1);
        let boundary = now.div_ceil(window) * window;
        boundary.saturating_add(ttl.as_secs())
    }

    /// Produce a token for `method` on `tail` valid until `expires`.
    pub fn sign(&self, method: &Method, expires: u64, tail: &str) -> String {
        let mac = mac(&self.keys[&self.current], method, expires, tail);
        format!(
            "{VERSION}.{}.{expires}.{}",
            self.current,
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        )
    }

    /// Verify `token` against `method` and `tail` at time `now`.
    pub fn verify(
        &self,
        token: &str,
        method: &Method,
        tail: &str,
        now: SystemTime,
    ) -> Result<(), SignatureError> {
        let parsed = Token::parse(token).ok_or(SignatureError::Invalid)?;
        let key = self
            .keys
            .get(parsed.key_id)
            .ok_or(SignatureError::Invalid)?;
        let sig = URL_SAFE_NO_PAD
            .decode(parsed.signature)
            .map_err(|_| SignatureError::Invalid)?;

        mac(key, method, parsed.expires, tail)
            .verify_slice(&sig)
            .map_err(|_| SignatureError::Invalid)?;

        // Only after the signature is known good do we reason about expiry,
        // so a forged token never learns anything from a 410.
        let now = unix(now);
        if parsed.expires < now {
            return Err(SignatureError::Expired);
        }
        // The ceiling is checked here too, not only when minting, so a
        // backend holding the key cannot mint around it. The window's
        // rounding is part of a legitimate expiry.
        let limit = now
            .saturating_add(self.window.as_secs())
            .saturating_add(self.max_ttl.as_secs());
        if parsed.expires > limit {
            return Err(SignatureError::TooLong);
        }
        Ok(())
    }
}

struct Token<'a> {
    key_id: &'a str,
    expires: u64,
    signature: &'a str,
}

impl<'a> Token<'a> {
    fn parse(token: &'a str) -> Option<Self> {
        let mut parts = token.split('.');
        let version = parts.next()?;
        let key_id = parts.next()?;
        let expires = parts.next()?;
        let signature = parts.next()?;
        if parts.next().is_some() || version != VERSION || key_id.is_empty() {
            return None;
        }
        // Reject leading zeros and signs so each expiry has one encoding.
        if expires.is_empty()
            || (expires.len() > 1 && expires.starts_with('0'))
            || !expires.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        Some(Self {
            key_id,
            expires: expires.parse().ok()?,
            signature,
        })
    }
}

/// The MAC of `METHOD "\n" expires "\n" tail`, fed in parts: this runs on
/// every public request, so the message is never assembled.
fn mac(key: &[u8], method: &Method, expires: u64, tail: &str) -> HmacSha256 {
    let method = if *method == Method::HEAD {
        &Method::GET
    } else {
        method
    };
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(method.as_str().as_bytes());
    mac.update(b"\n");
    mac.update(decimal(expires).as_bytes());
    mac.update(b"\n");
    mac.update(tail.as_bytes());
    mac
}

/// `n` in decimal, on the stack.
fn decimal(mut n: u64) -> impl std::ops::Deref<Target = str> {
    struct Digits([u8; 20], usize);
    impl std::ops::Deref for Digits {
        type Target = str;
        fn deref(&self) -> &str {
            std::str::from_utf8(&self.0[self.1..]).expect("ascii digits")
        }
    }
    let mut buf = [0u8; 20];
    let mut start = buf.len();
    loop {
        start -= 1;
        buf[start] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    Digits(buf, start)
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> Signer {
        let mut keys = HashMap::new();
        keys.insert("k1".to_string(), vec![1u8; 32]);
        keys.insert("k2".to_string(), vec![2u8; 32]);
        Signer {
            keys,
            current: "k1".to_string(),
            window: Duration::from_secs(3600),
            max_ttl: Duration::from_secs(7 * 24 * 3600),
        }
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn config_validation() {
        let good = base64::engine::general_purpose::STANDARD.encode([1u8; 32]);
        let build = |current: &str, keys: serde_json::Value, window: &str| {
            let config: SigningConfig = serde_json::from_value(serde_json::json!({
                "current_key": current, "keys": keys, "window": window
            }))
            .unwrap();
            Signer::from_config(&config).map(drop)
        };
        let with_ttl = |ttl: serde_json::Value| {
            let config: SigningConfig = serde_json::from_value(serde_json::json!({
                "current_key": "k1", "keys": {"k1": good}, "ttl": ttl
            }))
            .unwrap();
            Signer::from_config(&config).map(drop)
        };
        assert!(with_ttl(serde_json::json!({"max": "7d"})).is_ok());
        assert!(with_ttl(serde_json::json!({"max": "0s"})).is_err());
        assert!(with_ttl(serde_json::json!({"read": "8d"})).is_err());
        assert!(with_ttl(serde_json::json!({"metadata": "8d"})).is_err());
        assert!(with_ttl(serde_json::json!({"upload": "8d"})).is_err());
        assert!(with_ttl(serde_json::json!({"read": "8d", "max": "30d"})).is_ok());
        assert!(build("k1", serde_json::json!({"k1": good}), "1h").is_ok());
        assert!(build("k2", serde_json::json!({"k1": good}), "1h").is_err());
        assert!(build("k1", serde_json::json!({"k1": good}), "0s").is_err());
        assert!(build("a.b", serde_json::json!({"a.b": good}), "1h").is_err());
        assert!(build("a/b", serde_json::json!({"a/b": good}), "1h").is_err());
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        assert!(build("k1", serde_json::json!({"k1": short}), "1h").is_err());
        assert!(build("k1", serde_json::json!({"k1": "not base64!"}), "1h").is_err());
    }

    #[test]
    fn round_trip() {
        let s = signer();
        let tail = "/abc/original/photo.jpg?disposition=attachment";
        let token = s.sign(&Method::GET, 2_000_000_000, tail);
        assert!(token.starts_with("v1.k1.2000000000."));
        let now = at(2_000_000_000 - 1_000);
        assert_eq!(s.verify(&token, &Method::GET, tail, now), Ok(()));
        // HEAD is verified as GET.
        assert_eq!(s.verify(&token, &Method::HEAD, tail, now), Ok(()));
    }

    #[test]
    fn rejects_tampering() {
        let s = signer();
        let tail = "/abc/original/photo.jpg";
        let token = s.sign(&Method::GET, 2_000_000_000, tail);
        assert_eq!(
            s.verify(&token, &Method::GET, "/abc/original/photo.jpg?x=1", at(0)),
            Err(SignatureError::Invalid)
        );
        assert_eq!(
            s.verify(&token, &Method::PUT, tail, at(0)),
            Err(SignatureError::Invalid)
        );
        let forged = token.replace("2000000000", "3000000000");
        assert_eq!(
            s.verify(&forged, &Method::GET, tail, at(0)),
            Err(SignatureError::Invalid)
        );
        assert_eq!(
            s.verify("v1.k1.2000000000.AAAA", &Method::GET, tail, at(0)),
            Err(SignatureError::Invalid)
        );
        assert_eq!(
            s.verify("garbage", &Method::GET, tail, at(0)),
            Err(SignatureError::Invalid)
        );
    }

    #[test]
    fn expiry() {
        let s = signer();
        let token = s.sign(&Method::GET, 1_000, "/abc");
        assert_eq!(s.verify(&token, &Method::GET, "/abc", at(1_000)), Ok(()));
        assert_eq!(
            s.verify(&token, &Method::GET, "/abc", at(1_001)),
            Err(SignatureError::Expired)
        );
    }

    #[test]
    fn expiry_is_capped_at_verification() {
        let s = signer();
        let week = 7 * 24 * 3600;
        // What a mint at `now` could yield at most: the next window, then
        // the maximum TTL.
        let token = s.sign(&Method::GET, 3_600 + week, "/abc");
        assert_eq!(s.verify(&token, &Method::GET, "/abc", at(0)), Ok(()));
        let token = s.sign(&Method::GET, 3_600 + week + 1, "/abc");
        assert_eq!(
            s.verify(&token, &Method::GET, "/abc", at(0)),
            Err(SignatureError::TooLong)
        );
        // A token minted past the ceiling becomes valid once enough time
        // has passed, and is never permanent.
        assert_eq!(s.verify(&token, &Method::GET, "/abc", at(1)), Ok(()));
        let token = s.sign(&Method::GET, 0, "/abc");
        assert_eq!(
            s.verify(&token, &Method::GET, "/abc", at(1)),
            Err(SignatureError::Expired)
        );
    }

    #[test]
    fn other_keys_verify() {
        let mut s = signer();
        let token = s.sign(&Method::GET, 2_000_000_000, "/abc");
        s.current = "k2".to_string();
        let now = at(2_000_000_000 - 1);
        assert_eq!(s.verify(&token, &Method::GET, "/abc", now), Ok(()));
        let token2 = s.sign(&Method::GET, 2_000_000_000, "/abc");
        assert!(token2.starts_with("v1.k2."));
        assert_ne!(token, token2);
    }

    #[test]
    fn expiry_is_rounded_to_window() {
        let s = signer();
        let ttl = Duration::from_secs(600);
        let a = s.expiry_for(ttl, at(3_601));
        let b = s.expiry_for(ttl, at(7_199));
        assert_eq!(a, b);
        assert_eq!(a, 7_200 + 600);
        assert_eq!(s.expiry_for(ttl, at(7_200)), 7_200 + 600);
        assert_eq!(s.expiry_for(ttl, at(7_201)), 10_800 + 600);
        assert_eq!(s.expiry_for(Duration::MAX, at(7_201)), u64::MAX);
    }
}
