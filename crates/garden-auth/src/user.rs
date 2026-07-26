//! User accounts.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(pub Uuid);

impl UserId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for UserId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for UserId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// A normalised email address.
///
/// Normalisation is not cosmetic here: sharing works by inviting an address, so
/// `Phil@Example.COM` and `phil@example.com` must resolve to the same account or an
/// invitation silently goes nowhere.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EmailAddress(String);

impl EmailAddress {
    pub fn parse(raw: &str) -> Result<Self, InvalidEmail> {
        let trimmed = raw.trim();
        if trimmed.len() > 254 {
            return Err(InvalidEmail::TooLong);
        }

        let (local, domain) = trimmed.split_once('@').ok_or(InvalidEmail::MissingAt)?;
        if local.is_empty() {
            return Err(InvalidEmail::EmptyLocalPart);
        }
        if domain.is_empty() || !domain.contains('.') {
            return Err(InvalidEmail::InvalidDomain);
        }
        if trimmed.matches('@').count() != 1 {
            return Err(InvalidEmail::MissingAt);
        }
        if trimmed.chars().any(char::is_whitespace) {
            return Err(InvalidEmail::ContainsWhitespace);
        }

        Ok(Self(trimmed.to_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EmailAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidEmail {
    #[error("an email address needs exactly one '@'")]
    MissingAt,
    #[error("an email address needs something before the '@'")]
    EmptyLocalPart,
    #[error("that domain does not look like a domain")]
    InvalidDomain,
    #[error("an email address cannot contain spaces")]
    ContainsWhitespace,
    #[error("that email address is too long")]
    TooLong,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub email: EmailAddress,
    pub display_name: String,
    /// Server administrator.
    ///
    /// Grants access to fleet and system health only. Deliberately **not** a
    /// backdoor into other people's gardens — see [`crate::actor::Actor::can`].
    pub is_admin: bool,
    pub created_at: jiff::Timestamp,
    /// Set when the account is suspended. A disabled user keeps their memberships so
    /// that re-enabling them restores access, but cannot authenticate.
    pub disabled_at: Option<jiff::Timestamp>,
}

impl User {
    pub fn new(
        email: EmailAddress,
        display_name: impl Into<String>,
        created_at: jiff::Timestamp,
    ) -> Self {
        Self {
            id: UserId::new(),
            email,
            display_name: display_name.into(),
            is_admin: false,
            created_at,
            disabled_at: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.disabled_at.is_none()
    }

    /// Name to show in the UI, falling back to the address if none was given.
    pub fn label(&self) -> &str {
        if self.display_name.trim().is_empty() {
            self.email.as_str()
        } else {
            &self.display_name
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> jiff::Timestamp {
        jiff::Timestamp::from_second(1_700_000_000).unwrap()
    }

    #[test]
    fn addresses_are_case_folded_so_invitations_land() {
        let a = EmailAddress::parse("Phil@Example.COM").unwrap();
        let b = EmailAddress::parse("phil@example.com").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.as_str(), "phil@example.com");
    }

    #[test]
    fn surrounding_whitespace_is_forgiven() {
        assert_eq!(
            EmailAddress::parse("  phil@example.com \t").unwrap().as_str(),
            "phil@example.com"
        );
    }

    #[test]
    fn obviously_broken_addresses_are_rejected() {
        for bad in [
            "no-at-sign",
            "@example.com",
            "phil@",
            "phil@localhost",
            "two@at@example.com",
            "phil smith@example.com",
        ] {
            assert!(
                EmailAddress::parse(bad).is_err(),
                "{bad} should not have parsed"
            );
        }
    }

    #[test]
    fn absurdly_long_addresses_are_rejected() {
        let long = format!("{}@example.com", "a".repeat(300));
        assert_eq!(EmailAddress::parse(&long), Err(InvalidEmail::TooLong));
    }

    #[test]
    fn a_user_falls_back_to_their_address_for_display() {
        let email = EmailAddress::parse("phil@example.com").unwrap();
        let mut user = User::new(email, "   ", t0());
        assert_eq!(user.label(), "phil@example.com");
        user.display_name = "Phil".into();
        assert_eq!(user.label(), "Phil");
    }

    #[test]
    fn disabling_a_user_does_not_erase_them() {
        let email = EmailAddress::parse("phil@example.com").unwrap();
        let mut user = User::new(email, "Phil", t0());
        assert!(user.is_active());
        user.disabled_at = Some(t0());
        assert!(!user.is_active());
    }
}
