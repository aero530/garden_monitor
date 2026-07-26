//! The Gardyn web server.
//!
//! One self-contained binary: SQLite for storage, server-rendered HTML, no external
//! services in the runtime path. Configuration comes from the environment so it drops
//! into a Podman Quadlet unit without a config file.

mod api;
mod app;
mod demo;
mod error;
mod pages;
mod render;
mod state;
mod ui;

use app::{AppState, Config};
use axum::Router;
use gardyn_store::Store;
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gardyn_web=info,tower_http=warn".into()),
        )
        .init();

    let database = std::env::var("GARDYN_DB").unwrap_or_else(|_| "sqlite://gardyn.db".into());
    let bind = std::env::var("GARDYN_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let base_url = std::env::var("GARDYN_BASE_URL").unwrap_or_else(|_| format!("http://{bind}"));
    // Defaults to on. Turning it off has to be the explicit choice, because a
    // `__Host-` cookie over plain HTTP fails in a way that looks like a broken login
    // rather than a misconfiguration.
    let secure_cookies = std::env::var("GARDYN_INSECURE_COOKIES").is_err();
    let agent_token = std::env::var("GARDYN_AGENT_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());

    if agent_token.is_none() {
        tracing::warn!("GARDYN_AGENT_TOKEN is unset — the agent API is closed");
    }
    if !secure_cookies {
        tracing::warn!("GARDYN_INSECURE_COOKIES is set — cookies will not be marked Secure");
    }

    // Frame bytes live on disk, not in SQLite, so backups stay small.
    let frame_root = std::env::var("GARDYN_DATA_DIR")
        .unwrap_or_else(|_| "gardyn-data".into())
        + "/frames";

    let store = Store::open_with(&database, &frame_root).await?;
    tracing::info!("camera frames stored under {frame_root}");
    if store.user_count().await? == 0 {
        tracing::info!("no accounts yet — the first to register becomes administrator");
    }

    let state = AppState::new(
        store,
        Config {
            secure_cookies,
            base_url: base_url.clone(),
            agent_token,
        },
    );

    let router = Router::new()
        .merge(pages::gardens::routes())
        .merge(pages::auth::routes())
        .merge(pages::members::routes())
        .merge(pages::frames::routes())
        .merge(pages::slots::routes())
        .merge(pages::tasks::routes())
        .merge(pages::fleet::routes())
        .merge(api::routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("listening on {bind} (base url {base_url})");
    axum::serve(listener, router).await?;
    Ok(())
}
