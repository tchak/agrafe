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

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use http::Method;
use sha2::Sha256;

use crate::config::SigningConfig;

type HmacSha256 = Hmac<Sha256>;

const VERSION: &str = "v1";

/// `expires_unix` value meaning "never".
pub const PERMANENT: u64 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    #[error("invalid signature")]
    Invalid,
    #[error("signature expired")]
    Expired,
}

#[derive(Clone)]
pub struct Signer {
    keys: HashMap<String, Vec<u8>>,
    current: String,
    window: Duration,
    allow_permanent: bool,
}

impl Signer {
    pub fn from_config(config: &SigningConfig) -> Result<Self> {
        let mut keys = HashMap::new();
        for (id, secret) in &config.keys {
            keys.insert(
                id.clone(),
                secret
                    .decoded()
                    .with_context(|| format!("signing key `{id}`"))?,
            );
        }
        anyhow::ensure!(
            keys.contains_key(&config.current_key),
            "current signing key is missing"
        );
        Ok(Self {
            keys,
            current: config.current_key.clone(),
            window: config.window,
            allow_permanent: config.allow_permanent,
        })
    }

    pub fn allow_permanent(&self) -> bool {
        self.allow_permanent
    }

    /// Expiry for a URL minted now: rounded up to the next window boundary,
    /// then the TTL is added. Every mint inside one window yields the same value.
    pub fn expiry_for(&self, ttl: Duration, now: SystemTime) -> u64 {
        let now = unix(now);
        let window = self.window.as_secs().max(1);
        let boundary = now.div_ceil(window) * window;
        boundary.saturating_add(ttl.as_secs())
    }

    /// Produce a token for `method` on `tail` valid until `expires`.
    pub fn sign(&self, method: &Method, expires: u64, tail: &str) -> String {
        let key = &self.keys[&self.current];
        let mac = compute(key, method, expires, tail);
        format!(
            "{VERSION}.{}.{expires}.{}",
            self.current,
            URL_SAFE_NO_PAD.encode(mac)
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

        let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
        mac.update(message(method, parsed.expires, tail).as_bytes());
        mac.verify_slice(&sig)
            .map_err(|_| SignatureError::Invalid)?;

        // Only after the signature is known good do we reason about expiry,
        // so a forged token never learns anything from a 410.
        if parsed.expires == PERMANENT {
            return if self.allow_permanent {
                Ok(())
            } else {
                Err(SignatureError::Invalid)
            };
        }
        if parsed.expires < unix(now) {
            return Err(SignatureError::Expired);
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

fn message(method: &Method, expires: u64, tail: &str) -> String {
    let method = if *method == Method::HEAD {
        Method::GET
    } else {
        method.clone()
    };
    format!("{method}\n{expires}\n{tail}")
}

fn compute(key: &[u8], method: &Method, expires: u64, tail: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(message(method, expires, tail).as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer(allow_permanent: bool) -> Signer {
        let mut keys = HashMap::new();
        keys.insert("k1".to_string(), vec![1u8; 32]);
        keys.insert("k2".to_string(), vec![2u8; 32]);
        Signer {
            keys,
            current: "k1".to_string(),
            window: Duration::from_secs(3600),
            allow_permanent,
        }
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn round_trip() {
        let s = signer(false);
        let tail = "/abc/original/photo.jpg?disposition=attachment";
        let token = s.sign(&Method::GET, 2_000_000_000, tail);
        assert!(token.starts_with("v1.k1.2000000000."));
        assert_eq!(s.verify(&token, &Method::GET, tail, at(1_000)), Ok(()));
        // HEAD is verified as GET.
        assert_eq!(s.verify(&token, &Method::HEAD, tail, at(1_000)), Ok(()));
    }

    #[test]
    fn rejects_tampering() {
        let s = signer(false);
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
        let s = signer(false);
        let token = s.sign(&Method::GET, 1_000, "/abc");
        assert_eq!(s.verify(&token, &Method::GET, "/abc", at(1_000)), Ok(()));
        assert_eq!(
            s.verify(&token, &Method::GET, "/abc", at(1_001)),
            Err(SignatureError::Expired)
        );
    }

    #[test]
    fn permanent_requires_opt_in() {
        let token = signer(false).sign(&Method::GET, PERMANENT, "/abc");
        assert_eq!(
            signer(false).verify(&token, &Method::GET, "/abc", at(5)),
            Err(SignatureError::Invalid)
        );
        assert_eq!(
            signer(true).verify(&token, &Method::GET, "/abc", at(5)),
            Ok(())
        );
    }

    #[test]
    fn other_keys_verify() {
        let mut s = signer(false);
        let token = s.sign(&Method::GET, 2_000_000_000, "/abc");
        s.current = "k2".to_string();
        assert_eq!(s.verify(&token, &Method::GET, "/abc", at(0)), Ok(()));
        let token2 = s.sign(&Method::GET, 2_000_000_000, "/abc");
        assert!(token2.starts_with("v1.k2."));
        assert_ne!(token, token2);
    }

    #[test]
    fn expiry_is_rounded_to_window() {
        let s = signer(false);
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
