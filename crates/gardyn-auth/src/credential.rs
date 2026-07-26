//! Password hashing and policy.

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rand::Rng;
use serde::{Deserialize, Serialize};

/// A stored Argon2id PHC string.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PasswordDigest(String);

impl PasswordDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn from_stored(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Debug for PasswordDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordDigest(redacted)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("could not hash the password: {0}")]
    Hashing(String),
    #[error(transparent)]
    Policy(#[from] WeakPassword),
}

/// Hash a password with Argon2id and a fresh random salt.
///
/// The salt is generated here rather than via `SaltString::generate` because the
/// `rand_core` that `password-hash` re-exports is built without `getrandom`, so its
/// `OsRng` is unavailable. Sixteen bytes from the OS CSPRNG is the same thing.
pub fn hash_password(password: &str) -> Result<PasswordDigest, PasswordError> {
    let mut salt_bytes = [0u8; 16];
    rand::rng().fill(&mut salt_bytes[..]);
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| PasswordError::Hashing(e.to_string()))?;

    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| PasswordDigest(h.to_string()))
        .map_err(|e| PasswordError::Hashing(e.to_string()))
}

/// Check a password against a stored digest.
///
/// Returns plain `bool` rather than a `Result` on purpose: a malformed stored hash
/// and a wrong password must be indistinguishable to the caller, so neither can be
/// used to probe account state.
pub fn verify_password(password: &str, digest: &PasswordDigest) -> bool {
    let Ok(parsed) = PasswordHash::new(&digest.0) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WeakPassword {
    #[error("passwords must be at least {MIN_PASSWORD_LENGTH} characters")]
    TooShort,
    #[error("that password is too long")]
    TooLong,
    #[error("that password is too common — pick something else")]
    TooCommon,
}

pub const MIN_PASSWORD_LENGTH: usize = 12;
/// Argon2 has no practical input limit, but an unbounded password is a cheap way to
/// make the server do expensive work.
pub const MAX_PASSWORD_LENGTH: usize = 1024;

/// A short list of passwords that are guessed first, every time.
const OBVIOUS: &[&str] = &[
    "password", "password123", "123456789012", "qwertyuiop", "letmein1234",
    "changeme", "gardenpassword", "administrator", "iloveyou123",
];

pub fn check_password_policy(password: &str) -> Result<(), WeakPassword> {
    if password.chars().count() < MIN_PASSWORD_LENGTH {
        return Err(WeakPassword::TooShort);
    }
    if password.len() > MAX_PASSWORD_LENGTH {
        return Err(WeakPassword::TooLong);
    }
    let folded = password.to_lowercase();
    if OBVIOUS.contains(&folded.as_str()) {
        return Err(WeakPassword::TooCommon);
    }
    Ok(())
}

/// Validate then hash, for the registration and password-change paths.
pub fn accept_new_password(password: &str) -> Result<PasswordDigest, PasswordError> {
    check_password_policy(password)?;
    hash_password(password)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_verifies_against_its_own_hash() {
        let digest = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &digest));
    }

    #[test]
    fn a_wrong_password_does_not_verify() {
        let digest = hash_password("correct horse battery staple").unwrap();
        assert!(!verify_password("Correct horse battery staple", &digest));
        assert!(!verify_password("", &digest));
        assert!(!verify_password("correct horse battery stapl", &digest));
    }

    #[test]
    fn the_same_password_hashes_differently_every_time() {
        // A shared salt would let an attacker spot two accounts with one password.
        let a = hash_password("correct horse battery staple").unwrap();
        let b = hash_password("correct horse battery staple").unwrap();
        assert_ne!(a.as_str(), b.as_str());
        assert!(verify_password("correct horse battery staple", &a));
        assert!(verify_password("correct horse battery staple", &b));
    }

    #[test]
    fn the_hash_is_argon2id() {
        let digest = hash_password("correct horse battery staple").unwrap();
        assert!(digest.as_str().starts_with("$argon2id$"), "{digest:?}");
    }

    #[test]
    fn a_corrupt_stored_hash_reads_as_a_failed_login_not_a_crash() {
        let junk = PasswordDigest::from_stored("not a PHC string");
        assert!(!verify_password("anything", &junk));
        assert!(!verify_password("", &junk));
    }

    #[test]
    fn a_digest_never_leaks_through_debug() {
        let digest = hash_password("correct horse battery staple").unwrap();
        let rendered = format!("{digest:?}");
        assert!(!rendered.contains("argon2"));
        assert_eq!(rendered, "PasswordDigest(redacted)");
    }

    #[test]
    fn short_passwords_are_refused() {
        assert_eq!(check_password_policy("short"), Err(WeakPassword::TooShort));
        assert_eq!(
            check_password_policy(&"a".repeat(MIN_PASSWORD_LENGTH - 1)),
            Err(WeakPassword::TooShort)
        );
        assert!(check_password_policy(&"a".repeat(MIN_PASSWORD_LENGTH)).is_ok());
    }

    #[test]
    fn unbounded_passwords_are_refused_rather_than_hashed() {
        assert_eq!(
            check_password_policy(&"a".repeat(MAX_PASSWORD_LENGTH + 1)),
            Err(WeakPassword::TooLong)
        );
    }

    #[test]
    fn obvious_passwords_are_refused_case_insensitively() {
        // Long enough to pass the length check, so the common-password check is what
        // rejects it.
        assert_eq!(
            check_password_policy("GardenPassword"),
            Err(WeakPassword::TooCommon)
        );
        assert_eq!(
            check_password_policy("123456789012"),
            Err(WeakPassword::TooCommon)
        );
    }

    #[test]
    fn length_is_reported_before_commonness() {
        // "changeme" is both short and obvious; the more actionable message wins.
        assert_eq!(check_password_policy("changeme"), Err(WeakPassword::TooShort));
    }

    #[test]
    fn multibyte_passwords_are_measured_in_characters_not_bytes() {
        // Twelve emoji is a long password, not a short one.
        assert!(check_password_policy(&"🌱".repeat(12)).is_ok());
        assert_eq!(
            check_password_policy(&"🌱".repeat(4)),
            Err(WeakPassword::TooShort)
        );
    }

    #[test]
    fn registration_validates_before_hashing() {
        assert!(accept_new_password("short").is_err());
        assert!(accept_new_password("a long enough password").is_ok());
    }
}
