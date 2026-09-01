//! Machine-facing endpoints for agents.
//!
//! Agents are not people: they hold a shared bearer token from the environment rather
//! than a session. Keeping them on a separate surface means an agent credential can
//! never be mistaken for a user credential, and the blast radius of a leaked token on
//! a Pi is "can report fake health", not "can read someone's garden".

use crate::app::AppState;
use crate::error::AppError;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use axum::{Json, Router, routing::get, routing::post};
use garden_core::GardenId;
use garden_store::frames::{FrameSource, MAX_FRAME_BYTES, NewFrame};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/components/register", post(register))
        .route("/api/components/{id}/heartbeat", post(heartbeat))
        .route("/api/gardens/{id}/telemetry", post(telemetry))
        .route(
            "/api/gardens/{id}/frames",
            post(upload_frame)
                // Bound the body before it is buffered, so a runaway agent cannot
                // exhaust memory ahead of the size check in the handler.
                .layer(DefaultBodyLimit::max(MAX_FRAME_BYTES)),
        )
}

/// Accept a sensor sample from an edge agent.
///
/// The brain keeps the pump baseline rather than the agent, because the clean-system
/// reference outlives any single agent run — a Pi that reboots must not reset the
/// fouling trend that "time to clean" depends on.
async fn telemetry(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(report): Json<garden_proto::TelemetryReport>,
) -> Result<Json<garden_proto::TelemetryAccepted>, AppError> {
    authorize_agent(&state, &headers)?;

    let garden: GardenId = id
        .parse()
        .map_err(|_| AppError::bad_request("garden must be a valid id"))?;
    if state.store.find_garden(garden).await?.is_none() {
        return Err(AppError::NotFound);
    }

    if report.protocol > garden_proto::PROTOCOL_VERSION {
        return Err(AppError::bad_request(format!(
            "agent speaks protocol {} but this server understands {}",
            report.protocol,
            garden_proto::PROTOCOL_VERSION
        )));
    }

    state
        .store
        .record_reading(garden, &report.sensors, Some(&report.agent_version))
        .await?;

    // Echoed back so an agent can see what the brain inferred, which is the quickest
    // way to notice a probe that is wired up but reading nothing.
    let capabilities = report
        .sensors
        .capabilities()
        .iter()
        .map(|c| c.label().to_string())
        .collect();

    // The schedule rides back on the response the agent already asked for. No second
    // endpoint, no inbound connection for a firewall to allow, and an agent that
    // cannot reach us simply keeps the one it has — which is the required behaviour
    // rather than a fallback.
    let schedule = state.store.schedule(garden).await?;

    Ok(Json(garden_proto::TelemetryAccepted {
        capabilities,
        schedule,
    }))
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

#[derive(Serialize)]
pub struct FrameResponse {
    id: String,
    url: String,
}

/// Accept a camera frame from an edge agent.
///
/// The body is the raw image; metadata rides in headers so the agent never has to
/// build a multipart payload on a Pi Zero. Notably the agent does *not* get to declare
/// the content type — the bytes are sniffed, because a claimed `image/jpeg` carrying
/// HTML would end up served from our own origin.
async fn upload_frame(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<FrameResponse>, AppError> {
    authorize_agent(&state, &headers)?;

    let garden: GardenId = id
        .parse()
        .map_err(|_| AppError::bad_request("garden must be a valid id"))?;
    // Confirm the garden exists before writing a file for it. The model is needed too:
    // it decides which way up the camera is mounted.
    let Some(record) = state.store.find_garden(garden).await? else {
        return Err(AppError::NotFound);
    };

    let header_i64 = |name: &str| -> Option<i64> {
        headers.get(name)?.to_str().ok()?.trim().parse().ok()
    };
    let captured_at = headers
        .get("x-captured-at")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<jiff::Timestamp>().ok())
        .unwrap_or_else(|| state.now());
    let light_duty_milli = header_i64("x-light-duty-milli").map(|d| d.clamp(0, 1000));
    // Only the agent knows whether it pinned the lights before shooting, and only
    // frames that did are comparable with each other.
    let comparable = headers
        .get("x-photo-mode")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes"));

    // Turned the right way up before anything else sees it.
    //
    // The Studio 2's camera lies sideways in the light bar, so a frame arrives with the
    // towers horizontal. Rotating here rather than in each consumer means the stored
    // file, the recorded dimensions, the ROI map and the dashboard are all in one
    // coordinate space — and that the Pi, which would have to decode and re-encode a
    // JPEG on a single ARMv6 core, never has to touch it.
    //
    // The agent's own width and height headers describe the frame as shot, so they are
    // taken from the rotated image instead: believing the headers would leave a 1080×1920
    // file recorded as 1920×1080, and `roi` rejects a frame whose size disagrees with the
    // map rather than rescaling it.
    let rotation = record.model.camera_rotation();
    let (bytes, width, height) = match garden_vision::orient::orient(&body, rotation) {
        Ok(oriented) => (
            std::borrow::Cow::Owned(oriented.bytes),
            oriented.width,
            oriented.height,
        ),
        // An undecodable body is still worth keeping: `put_frame` sniffs the bytes and
        // will reject it if it is not an image at all, and that is a better place for
        // that judgement than here.
        Err(e) => {
            tracing::warn!(%garden, %e, "could not orient the frame; storing it as shot");
            let width = header_i64("x-width").unwrap_or(0).clamp(0, 100_000) as u32;
            let height = header_i64("x-height").unwrap_or(0).clamp(0, 100_000) as u32;
            (std::borrow::Cow::Borrowed(body.as_ref()), width, height)
        }
    };

    let stored = state
        .store
        .put_frame(NewFrame {
            garden,
            captured_at,
            width,
            height,
            light_duty_milli,
            comparable,
            source: FrameSource::Agent,
            bytes: &bytes,
        })
        .await?;

    match stored {
        Ok(frame) => {
            // Measured here, while the bytes are in memory. A failure inside this call
            // is logged and swallowed: the photograph is worth keeping even when the
            // pipeline cannot read it, and an agent that gets a 500 for an unanalysable
            // frame will retry it forever.
            if let Some(measured) =
                // The rotated bytes, not `body`. The ROI map is in the upright frame's
                // coordinate space, so analysing the frame as shot would measure the
                // wrong rectangle for every slot — and silently, because a plausible
                // canopy area comes back either way.
                crate::vision::analyse_and_store(
                    &state.store,
                    garden,
                    frame.id,
                    &bytes,
                    captured_at,
                )
                .await
            {
                tracing::debug!(%garden, measured, "frame analysed");
            }
            Ok(Json(FrameResponse {
                id: frame.id.to_string(),
                url: format!("{}{}", state.config.base_url, frame.image_path()),
            }))
        }
        Err(rejected) => Err(AppError::bad_request(rejected.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Config;
    use axum::http::HeaderValue;
    use garden_store::Store;

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

    /// A landscape JPEG, the shape the Studio 2's camera actually produces.
    fn landscape_jpeg() -> Bytes {
        let img = image::RgbImage::from_pixel(320, 180, image::Rgb([20, 120, 40]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
            .encode_image(&image::DynamicImage::ImageRgb8(img))
            .unwrap();
        Bytes::from(out)
    }

    async fn upload_to(model: garden_core::DeviceModel) -> garden_store::frames::Frame {
        let state = state_with(Some("t")).await;
        let owner = state
            .store
            .create_user(
                garden_auth::EmailAddress::parse("phil@example.com").unwrap(),
                "Phil",
                "a long enough password",
                state.now(),
            )
            .await
            .unwrap();
        let garden = state
            .store
            .create_garden("Kitchen", model, "UTC", owner.id, state.now())
            .await
            .unwrap();
        let mut headers = bearer("t");
        // Deliberately the dimensions *as shot*, which is what the agent sends.
        headers.insert("x-width", HeaderValue::from_static("320"));
        headers.insert("x-height", HeaderValue::from_static("180"));
        let _accepted = upload_frame(
            State(state.clone()),
            Path(garden.id.to_string()),
            headers,
            landscape_jpeg(),
        )
        .await
        .expect("the upload should succeed");
        state.store.latest_frame(garden.id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_studio_2_frame_is_stored_upright() {
        // The camera lies sideways in the light bar, so 320×180 as shot must be recorded
        // as 180×320. Believing the agent's headers instead would leave a portrait file
        // described as landscape, and `roi` rejects a frame whose size disagrees with
        // the map rather than rescaling it — so every slot measurement would stop.
        let frame = upload_to(garden_core::DeviceModel::Studio2).await;
        assert_eq!((frame.width, frame.height), (180, 320));
    }

    #[tokio::test]
    async fn a_home_frame_is_stored_exactly_as_shot() {
        // The Home line mounts its cameras upright, and a rotation of zero must not cost
        // a lossy re-encode.
        let frame = upload_to(garden_core::DeviceModel::Home4).await;
        assert_eq!((frame.width, frame.height), (320, 180));
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
