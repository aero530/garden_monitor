//! The Garden web server.
//!
//! One self-contained binary: SQLite for storage, server-rendered HTML, no external
//! services in the runtime path. Configuration comes from the environment so it drops
//! into a Podman Quadlet unit without a config file.

mod api;
mod app;
mod demo;
mod dispatch;
mod error;
mod pages;
mod render;
mod retention;
mod state;
mod ui;
mod vision;

use app::{AppState, Config};
use axum::Router;
use garden_store::Store;
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "garden_web=info,tower_http=warn".into()),
        )
        .init();

    let database = std::env::var("GARDEN_DB").unwrap_or_else(|_| "sqlite://garden.db".into());
    let bind = std::env::var("GARDEN_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let base_url = std::env::var("GARDEN_BASE_URL").unwrap_or_else(|_| format!("http://{bind}"));
    // Defaults to on. Turning it off has to be the explicit choice, because a
    // `__Host-` cookie over plain HTTP fails in a way that looks like a broken login
    // rather than a misconfiguration.
    let secure_cookies = std::env::var("GARDEN_INSECURE_COOKIES").is_err();
    let agent_token = std::env::var("GARDEN_AGENT_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());

    if agent_token.is_none() {
        tracing::warn!("GARDEN_AGENT_TOKEN is unset — the agent API is closed");
    }
    if !secure_cookies {
        tracing::warn!("GARDEN_INSECURE_COOKIES is set — cookies will not be marked Secure");
    }

    // Frame bytes live on disk, not in SQLite, so backups stay small.
    let frame_root = std::env::var("GARDEN_DATA_DIR")
        .unwrap_or_else(|_| "garden-data".into())
        + "/frames";

    let store = Store::open_with(&database, &frame_root).await?;
    tracing::info!("camera frames stored under {frame_root}");
    if store.user_count().await? == 0 {
        tracing::info!("no accounts yet — the first to register becomes administrator");
    }

    let notifier = build_notifier();
    if notifier.is_none() {
        tracing::warn!(
            "no notification channel configured — set GARDEN_NTFY_URL and/or              GARDEN_SMTP_HOST. Tasks will appear in the web UI but nothing will be sent."
        );
    }

    let state = AppState::new(
        store,
        Config {
            secure_cookies,
            base_url: base_url.clone(),
            agent_token,
        },
    )
    .with_notifier(notifier);

    dispatch::spawn(state.clone());
    retention::spawn(state.clone());

    let router = router(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("listening on {bind} (base url {base_url})");
    axum::serve(listener, router).await?;
    Ok(())
}

/// The whole route table.
///
/// Kept out of `main` so a test can build it. axum validates path patterns when the
/// router is assembled, not when it is compiled — a malformed one (`/x/{token}.ics`,
/// say) type-checks, passes every unit test, and then panics on the first start.
fn router(state: AppState) -> Router {
    Router::new()
        .merge(pages::gardens::routes())
        .merge(pages::auth::routes())
        .merge(pages::members::routes())
        .merge(pages::notify::routes())
        .merge(pages::frames::routes())
        .merge(pages::slots::routes())
        .merge(pages::storage::routes())
        .merge(pages::tasks::routes())
        .merge(pages::varieties::routes())
        .merge(pages::fleet::routes())
        .merge(api::routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Assemble whatever channels the environment configures.
///
/// Both are optional and independent. Push without working mail is the expected
/// self-hosted case, not a degraded one.
fn build_notifier() -> Option<garden_notify::Notifier> {
    let ntfy = std::env::var("GARDEN_NTFY_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .and_then(|base_url| {
            let config = garden_notify::NtfyConfig {
                base_url,
                token: std::env::var("GARDEN_NTFY_TOKEN").ok().filter(|t| !t.is_empty()),
            };
            match garden_notify::NtfyChannel::new(config) {
                Ok(channel) => Some(channel),
                Err(error) => {
                    tracing::error!(%error, "ntfy channel could not be built");
                    None
                }
            }
        });

    let email = std::env::var("GARDEN_SMTP_HOST")
        .ok()
        .filter(|h| !h.is_empty())
        .and_then(|host| {
            let config = garden_notify::EmailConfig {
                host,
                port: std::env::var("GARDEN_SMTP_PORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(587),
                username: std::env::var("GARDEN_SMTP_USER").ok().filter(|u| !u.is_empty()),
                password: std::env::var("GARDEN_SMTP_PASSWORD").ok().filter(|p| !p.is_empty()),
                from: std::env::var("GARDEN_SMTP_FROM")
                    .unwrap_or_else(|_| "garden@localhost".into()),
                starttls: std::env::var("GARDEN_SMTP_PLAINTEXT").is_err(),
            };
            match garden_notify::EmailChannel::new(config) {
                Ok(channel) => Some(channel),
                Err(error) => {
                    tracing::error!(%error, "smtp channel could not be built");
                    None
                }
            }
        });

    (ntfy.is_some() || email.is_some()).then(|| garden_notify::Notifier::new(ntfy, email))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The route table is only validated when it is built, so build it.
    ///
    /// This is the cheapest possible guard against a start-up panic: a path pattern
    /// axum rejects costs nothing to catch here and takes the whole service down
    /// otherwise.
    #[tokio::test]
    async fn every_route_pattern_is_valid() {
        let store = garden_store::Store::open_with(":memory:", std::env::temp_dir())
            .await
            .expect("in-memory store");
        let state = AppState::new(
            store,
            Config {
                secure_cookies: false,
                base_url: "http://localhost:8080".into(),
                agent_token: None,
            },
        );
        let _ = router(state);
    }
}
