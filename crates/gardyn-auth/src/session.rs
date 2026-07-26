//! Browser sessions.

use crate::token::{SecretToken, TokenDigest};
use crate::user::UserId;
use gardyn_core::time::{add_days, days_between};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Cookie name. `__Host-` prefixed so a browser refuses it unless it is secure,
/// host-scoped, and path `/` — which stops a subdomain from planting a session.
pub const SESSION_COOKIE: &str = "__Host-gardyn_session";

/// Cookie name used when serving plain HTTP, where `__Host-` is not accepted.
/// Only for LAN development; the deployment sits behind Tailscale or TLS.
pub const INSECURE_SESSION_COOKIE: &str = "gardyn_session";

pub const DEFAULT_LIFETIME_DAYS: f64 = 30.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub user: UserId,
    /// Digest of the cookie value. The secret itself is never stored.
    pub digest: TokenDigest,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub last_seen_at: Timestamp,
    /// Recorded for the "signed-in devices" list, so a user can spot a session they
    /// do not recognise. Not used for authorization — user agents are trivially forged.
    pub user_agent: Option<String>,
}

impl Session {
    /// Mint a session. The returned token goes to the browser and is never persisted.
    pub fn issue(user: UserId, now: Timestamp, user_agent: Option<String>) -> (Self, SecretToken) {
        let token = SecretToken::generate();
        let session = Self {
            id: SessionId::new(),
            user,
            digest: token.digest(),
            created_at: now,
            expires_at: add_days(now, DEFAULT_LIFETIME_DAYS),
            last_seen_at: now,
            user_agent,
        };
        (session, token)
    }

    pub fn is_valid(&self, now: Timestamp) -> bool {
        now < self.expires_at
    }

    pub fn age_days(&self, now: Timestamp) -> f64 {
        days_between(self.created_at, now)
    }

    pub fn touch(&mut self, now: Timestamp) {
        self.last_seen_at = now;
    }

    /// Cookie `Max-Age`, in seconds.
    pub fn max_age_seconds(&self, now: Timestamp) -> i64 {
        (self.expires_at.as_second() - now.as_second()).max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    #[test]
    fn a_fresh_session_is_valid_and_expires_later() {
        let (session, _) = Session::issue(UserId::new(), t0(), None);
        assert!(session.is_valid(t0()));
        assert!(session.is_valid(add_days(t0(), 29.0)));
        assert!(!session.is_valid(add_days(t0(), 31.0)));
    }

    #[test]
    fn the_stored_session_does_not_contain_the_cookie_value() {
        // A leaked database backup must not hand over live sessions.
        let (session, token) = Session::issue(UserId::new(), t0(), None);
        assert_ne!(session.digest.as_str(), token.expose());
        assert_eq!(session.digest, token.digest());
    }

    #[test]
    fn two_sessions_never_share_a_token() {
        let (_, a) = Session::issue(UserId::new(), t0(), None);
        let (_, b) = Session::issue(UserId::new(), t0(), None);
        assert_ne!(a.expose(), b.expose());
    }

    #[test]
    fn max_age_never_goes_negative() {
        let (session, _) = Session::issue(UserId::new(), t0(), None);
        assert!(session.max_age_seconds(t0()) > 0);
        assert_eq!(session.max_age_seconds(add_days(t0(), 60.0)), 0);
    }

    #[test]
    fn the_cookie_is_host_locked_by_default() {
        // `__Host-` makes the browser reject the cookie unless it is secure and
        // host-scoped, so a neighbouring subdomain cannot set one.
        assert!(SESSION_COOKIE.starts_with("__Host-"));
    }
}
