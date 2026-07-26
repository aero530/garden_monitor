//! Application state, cookies, and the authenticated-caller extractor.

use crate::error::AppError;
use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::request::Parts;
use garden_auth::{Actor, SecretToken, session};
use garden_store::Store;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Config {
    /// Whether to mark cookies `Secure` and use the `__Host-` prefixed name.
    ///
    /// True in any real deployment. False only for plain-HTTP LAN development, where
    /// a browser would silently refuse a `__Host-` cookie and the login loop would
    /// look mysteriously broken.
    pub secure_cookies: bool,
    /// Absolute base, used to build invite and notification links that have to work
    /// from a phone.
    pub base_url: String,
    /// Shared bearer token for edge agents. `None` closes the agent API entirely —
    /// an unset environment variable must not mean "anyone may report".
    pub agent_token: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            secure_cookies: false,
            base_url: "http://localhost:8080".into(),
            agent_token: None,
        }
    }
}

impl Config {
    pub fn cookie_name(&self) -> &'static str {
        if self.secure_cookies {
            session::SESSION_COOKIE
        } else {
            session::INSECURE_SESSION_COOKIE
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub config: Arc<Config>,
    /// `None` when no channel is configured. The web UI still works; nothing is sent.
    pub notifier: Option<Arc<garden_notify::Notifier>>,
}

impl AppState {
    pub fn new(store: Store, config: Config) -> Self {
        Self {
            store,
            config: Arc::new(config),
            notifier: None,
        }
    }

    #[must_use]
    pub fn with_notifier(mut self, notifier: Option<garden_notify::Notifier>) -> Self {
        self.notifier = notifier.map(Arc::new);
        self
    }

    pub fn now(&self) -> jiff::Timestamp {
        jiff::Timestamp::now()
    }
}

// --- Cookies --------------------------------------------------------------------

pub fn read_cookie<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// A `Set-Cookie` value.
///
/// `HttpOnly` so a script cannot read the session; `SameSite=Lax` so a cross-site
/// form post cannot ride it, while ordinary top-level navigation from a notification
/// still works.
pub fn set_cookie(name: &str, value: &str, max_age_seconds: i64, secure: bool) -> String {
    let mut cookie =
        format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_seconds}");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

pub fn clear_cookie(name: &str, secure: bool) -> String {
    set_cookie(name, "", 0, secure)
}

// --- Extractors -----------------------------------------------------------------

/// A signed-in caller. Handlers that take this cannot be reached anonymously.
pub struct Auth(pub Actor);

impl FromRequestParts<AppState> for Auth {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let MaybeAuth(actor) = MaybeAuth::from_request_parts(parts, state).await?;
        actor.map(Auth).ok_or(AppError::NotSignedIn)
    }
}

/// A caller who may or may not be signed in, for pages that render either way.
pub struct MaybeAuth(pub Option<Actor>);

impl FromRequestParts<AppState> for MaybeAuth {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(raw) = read_cookie(&parts.headers, state.config.cookie_name()) else {
            return Ok(MaybeAuth(None));
        };
        // Validate the shape before it reaches a database lookup.
        let Some(token) = SecretToken::from_client(raw) else {
            return Ok(MaybeAuth(None));
        };

        let actor = state.store.actor_for_token(&token, state.now()).await?;
        Ok(MaybeAuth(actor))
    }
}

/// A server administrator. Grants the system view, and nothing inside anyone's garden.
pub struct AdminAuth(pub Actor);

impl FromRequestParts<AppState> for AdminAuth {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Auth(actor) = Auth::from_request_parts(parts, state).await?;
        actor.require_admin()?;
        Ok(AdminAuth(actor))
    }
}

pub fn user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::USER_AGENT)?
        .to_str()
        .ok()
        .map(|s| s.chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(cookie: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_str(cookie).unwrap(),
        );
        headers
    }

    #[test]
    fn a_single_cookie_is_read() {
        let headers = headers_with("garden_session=abc123");
        assert_eq!(read_cookie(&headers, "garden_session"), Some("abc123"));
    }

    #[test]
    fn the_right_cookie_is_picked_out_of_several() {
        let headers = headers_with("theme=dark; garden_session=abc123; other=x");
        assert_eq!(read_cookie(&headers, "garden_session"), Some("abc123"));
        assert_eq!(read_cookie(&headers, "theme"), Some("dark"));
        assert_eq!(read_cookie(&headers, "absent"), None);
    }

    #[test]
    fn a_missing_cookie_header_is_not_an_error() {
        assert_eq!(read_cookie(&HeaderMap::new(), "garden_session"), None);
    }

    #[test]
    fn session_cookies_are_locked_down() {
        let cookie = set_cookie("garden_session", "abc", 3600, true);
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Lax"), "{cookie}");
        assert!(cookie.contains("Secure"), "{cookie}");
        assert!(cookie.contains("Path=/"), "{cookie}");
    }

    #[test]
    fn plain_http_omits_secure_so_local_development_works() {
        let cookie = set_cookie("garden_session", "abc", 3600, false);
        assert!(!cookie.contains("Secure"));
        assert!(cookie.contains("HttpOnly"));
    }

    #[test]
    fn clearing_a_cookie_expires_it_immediately() {
        assert!(clear_cookie("garden_session", true).contains("Max-Age=0"));
    }

    #[test]
    fn the_host_prefixed_name_is_used_only_when_cookies_are_secure() {
        // A browser rejects `__Host-` over plain HTTP, which would make login appear
        // to succeed and then silently fail.
        let secure = Config {
            secure_cookies: true,
            ..Default::default()
        };
        let insecure = Config::default();
        assert!(secure.cookie_name().starts_with("__Host-"));
        assert!(!insecure.cookie_name().starts_with("__Host-"));
    }
}
