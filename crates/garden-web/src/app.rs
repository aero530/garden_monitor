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

/// Whether session cookies may be marked `Secure`, given the URL browsers actually use.
///
/// Not a preference — a fact about the scheme. A `Secure` cookie is only accepted over
/// HTTPS, and the `__Host-` prefix additionally *requires* `Secure`, so issuing either
/// over `http://` produces a cookie every browser silently discards. The session then
/// never persists, and registering or signing in bounces back to the login page looking
/// exactly like a wrong password.
///
/// `https://` also covers the reverse-proxy case, where this process speaks plain HTTP
/// to a terminator that speaks TLS to the browser: what matters is the scheme the
/// *browser* used, and `base_url` is where that is recorded.
///
/// `forced_insecure` (`GARDEN_INSECURE_COOKIES`) can only ever turn this off. There is
/// nothing to be gained from letting it turn cookies on where they cannot work.
pub fn secure_cookies_for(base_url: &str, forced_insecure: bool) -> bool {
    !forced_insecure && base_url.trim_start().starts_with("https://")
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
    /// A pinned clock, for tests only.
    ///
    /// The retention sweep decides what is old by comparing against now, so a test
    /// that used the wall clock would have to write timestamps relative to whenever it
    /// happened to run. Pinning is clearer than arithmetic against `Timestamp::now()`
    /// in every assertion.
    #[cfg(test)]
    clock: Option<jiff::Timestamp>,
}

impl AppState {
    pub fn new(store: Store, config: Config) -> Self {
        Self {
            store,
            config: Arc::new(config),
            notifier: None,
            #[cfg(test)]
            clock: None,
        }
    }

    /// Pin the clock. Test-only, so production cannot accidentally stop time.
    #[cfg(test)]
    #[must_use]
    pub fn with_clock_at(mut self, at: jiff::Timestamp) -> Self {
        self.clock = Some(at);
        self
    }

    #[must_use]
    pub fn with_notifier(mut self, notifier: Option<garden_notify::Notifier>) -> Self {
        self.notifier = notifier.map(Arc::new);
        self
    }

    pub fn now(&self) -> jiff::Timestamp {
        #[cfg(test)]
        if let Some(pinned) = self.clock {
            return pinned;
        }
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

    /// The cookie a browser would actually be sent, end to end.
    fn issued_cookie(base_url: &str, forced_insecure: bool) -> String {
        let config = Config {
            secure_cookies: secure_cookies_for(base_url, forced_insecure),
            base_url: base_url.into(),
            agent_token: None,
        };
        set_cookie(config.cookie_name(), "tok", 60, config.secure_cookies)
    }

    #[test]
    fn a_plain_http_deployment_never_issues_a_cookie_the_browser_will_drop() {
        // The bug this exists to prevent, and it cost an evening. `__Host-` requires
        // `Secure`, and `Secure` requires HTTPS, so either one over http:// produces a
        // cookie every browser silently discards — after which registering succeeds,
        // the redirect arrives with no session, and the login page comes back looking
        // exactly like a wrong password.
        for base in [
            "http://192.168.86.10:8080",
            "http://garden-brain.local:8080",
            "http://localhost:8080",
        ] {
            let cookie = issued_cookie(base, false);
            assert!(!cookie.contains("Secure"), "{base} issued: {cookie}");
            assert!(!cookie.contains("__Host-"), "{base} issued: {cookie}");
        }
    }

    #[test]
    fn an_https_deployment_gets_the_hardened_cookie() {
        let cookie = issued_cookie("https://garden.example.com", false);
        assert!(cookie.contains("__Host-garden_session"), "{cookie}");
        assert!(cookie.contains("Secure"), "{cookie}");
    }

    #[test]
    fn the_override_can_only_weaken_never_strengthen() {
        // `GARDEN_INSECURE_COOKIES` exists for a TLS deployment someone wants to debug.
        // It must not be able to turn cookies *on* where they cannot work, or the
        // footgun comes back through the other door.
        assert!(!secure_cookies_for("https://garden.example.com", true));
        assert!(!secure_cookies_for("http://192.168.86.10:8080", true));
        assert!(!secure_cookies_for("http://192.168.86.10:8080", false));
        assert!(secure_cookies_for("https://garden.example.com", false));
    }

    #[test]
    fn a_scheme_that_is_not_https_is_not_treated_as_https() {
        // Nothing here should be fooled by a hostname that merely mentions it.
        assert!(!secure_cookies_for("http://https.example.com", false));
        assert!(!secure_cookies_for("garden.example.com", false));
        assert!(!secure_cookies_for("", false));
        // Leading whitespace from a hand-edited env file is not a scheme change.
        assert!(secure_cookies_for("  https://garden.example.com", false));
    }

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
