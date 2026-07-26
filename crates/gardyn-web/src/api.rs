//! Machine-facing endpoints for agents.
//!
//! Agents are not people: they hold a shared bearer token from the environment rather
//! than a session. Keeping them on a separate surface means an agent credential can
//! never be mistaken for a user credential, and the blast radius of a leaked token on
//! a Pi is "can report fake health", not "can read someone's garden".

use crate::app::AppState;
use crate::error::AppError;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use axum::{Json, Router, routing::get, routing::post};
use gardyn_core::GardenId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/components/register", post(register))
        .route("/api/components/{id}/heartbeat", post(heartbeat))
}

/// Constant-time-ish bearer check.
///
/// Compares length first and then folds every byte, so a wrong token does not leak
/// its correct prefix through response timing.
fn authorize_agent(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    let Some(expected) = state.config.agent_token.as_deref() else {
        return Err(AppError::Unauthorized);
    };
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    let matches = presented.len() == expected.len()
        && presented
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;

    if matches {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}

async fn healthz() -> (StatusCode, &'static str) {
    (StatusCode::OK, "ok")
}

#[derive(Deserialize)]
pub struct RegisterRequest {
    name: String,
    kind: String,
    garden: Option<String>,
    endpoint: Option<String>,
    heartbeat_seconds: Option<i64>,
}

#[derive(Serialize)]
pub struct RegisterResponse {
    id: String,
}

async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, AppError> {
    authorize_agent(&state, &headers)?;

    let garden = body
        .garden
        .as_deref()
        .map(|g| g.parse::<GardenId>())
        .transpose()
        .map_err(|_| AppError::bad_request("garden must be a valid id"))?;

    if body.name.trim().is_empty() {
        return Err(AppError::bad_request("name is required"));
    }

    let id = state
        .store
        .register_component(
            garden,
            body.name.trim(),
            &body.kind,
            body.heartbeat_seconds.unwrap_or(120).clamp(5, 86_400),
            body.endpoint.as_deref(),
            state.now(),
        )
        .await?;

    Ok(Json(RegisterResponse { id: id.to_string() }))
}

#[derive(Deserialize)]
pub struct HeartbeatRequest {
    /// "ok" means healthy; anything else is shown as the degraded reason.
    status: Option<String>,
    version: Option<String>,
    detail: Option<String>,
}

async fn heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<HeartbeatRequest>,
) -> Result<StatusCode, AppError> {
    authorize_agent(&state, &headers)?;
    let id = Uuid::parse_str(&id).map_err(|_| AppError::NotFound)?;

    state
        .store
        .heartbeat(
            id,
            body.status.as_deref().unwrap_or("ok"),
            body.version.as_deref(),
            body.detail.as_deref(),
            state.now(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Config;
    use axum::http::HeaderValue;
    use gardyn_store::Store;

    async fn state_with(token: Option<&str>) -> AppState {
        AppState::new(
            Store::in_memory().await.unwrap(),
            Config {
                agent_token: token.map(str::to_string),
                ..Default::default()
            },
        )
    }

    fn bearer(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {value}")).unwrap(),
        );
        headers
    }

    #[tokio::test]
    async fn the_right_token_is_accepted() {
        let state = state_with(Some("s3cret-agent-token")).await;
        assert!(authorize_agent(&state, &bearer("s3cret-agent-token")).is_ok());
    }

    #[tokio::test]
    async fn a_wrong_token_is_rejected() {
        let state = state_with(Some("s3cret-agent-token")).await;
        assert!(authorize_agent(&state, &bearer("wrong")).is_err());
        assert!(authorize_agent(&state, &bearer("s3cret-agent-toke")).is_err());
        assert!(authorize_agent(&state, &bearer("")).is_err());
    }

    #[tokio::test]
    async fn a_missing_header_is_rejected() {
        let state = state_with(Some("s3cret-agent-token")).await;
        assert!(authorize_agent(&state, &HeaderMap::new()).is_err());
    }

    #[tokio::test]
    async fn a_malformed_scheme_is_rejected() {
        let state = state_with(Some("s3cret-agent-token")).await;
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Basic s3cret-agent-token"),
        );
        assert!(authorize_agent(&state, &headers).is_err());
    }

    #[tokio::test]
    async fn agent_access_is_closed_when_no_token_is_configured() {
        // Failing closed matters: an unset environment variable must not mean
        // "anyone may report".
        let state = state_with(None).await;
        assert!(authorize_agent(&state, &bearer("")).is_err());
        assert!(authorize_agent(&state, &bearer("anything")).is_err());
    }
}
