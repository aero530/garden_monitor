//! Bearer secrets: session cookies, invitation links, and one-tap notification links.
//!
//! One rule governs all three: **the database stores a digest, never the secret.**
//! A leaked backup then yields no usable sessions and no working invite links. This
//! module exists so that rule lives in one place rather than being re-implemented,
//! slightly differently, three times.

use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

/// A high-entropy secret, shown to the client exactly once.
///
/// Deliberately does not implement `Display`, `Serialize`, or a revealing `Debug`:
/// the only way to get the string out is [`SecretToken::expose`], which is greppable
/// when auditing where secrets travel.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretToken(String);

impl SecretToken {
    /// 256 bits of OS randomness, hex encoded.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::rng().fill(&mut bytes[..]);
        Self(to_hex(&bytes))
    }

    /// Accept a token presented by a client.
    ///
    /// Rejects anything that is not exactly the shape we issue, so malformed input
    /// never reaches a database lookup.
    pub fn from_client(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let valid = trimmed.len() == 64 && trimmed.bytes().all(|b| b.is_ascii_hexdigit());
        valid.then(|| Self(trimmed.to_ascii_lowercase()))
    }

    /// The value to store and compare against.
    pub fn digest(&self) -> TokenDigest {
        let mut hasher = Sha256::new();
        hasher.update(self.0.as_bytes());
        TokenDigest(to_hex(&hasher.finalize()))
    }

    /// Hand the raw secret to a cookie, a URL, or a notification body.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never let a secret reach a log line through a derived Debug.
        f.write_str("SecretToken(redacted)")
    }
}

/// The stored form of a [`SecretToken`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenDigest(String);

impl TokenDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn from_stored(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl fmt::Display for TokenDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn to_hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_unique_and_well_formed() {
        let a = SecretToken::generate();
        let b = SecretToken::generate();
        assert_ne!(a.expose(), b.expose());
        assert_eq!(a.expose().len(), 64);
        assert!(a.expose().bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_digest_is_stable_and_does_not_reveal_the_secret() {
        let token = SecretToken::generate();
        assert_eq!(token.digest(), token.digest());
        assert_ne!(token.digest().as_str(), token.expose());
    }

    #[test]
    fn different_secrets_digest_differently() {
        assert_ne!(
            SecretToken::generate().digest(),
            SecretToken::generate().digest()
        );
    }

    #[test]
    fn a_secret_never_leaks_through_debug() {
        // Guards against a stray `tracing::debug!("{token:?}")` publishing a live
        // session cookie to the log.
        let token = SecretToken::generate();
        let rendered = format!("{token:?}");
        assert!(!rendered.contains(token.expose()));
        assert_eq!(rendered, "SecretToken(redacted)");
    }

    #[test]
    fn client_input_is_validated_before_it_reaches_a_lookup() {
        let real = SecretToken::generate();
        assert_eq!(SecretToken::from_client(real.expose()), Some(real.clone()));
        // Surrounding whitespace and casing are forgiven.
        let padded = format!("  {}  ", real.expose().to_uppercase());
        assert_eq!(SecretToken::from_client(&padded), Some(real));

        for bad in ["", "short", &"g".repeat(64), &"a".repeat(63), "' OR 1=1 --"] {
            assert!(
                SecretToken::from_client(bad).is_none(),
                "{bad:?} should have been rejected"
            );
        }
    }
}
