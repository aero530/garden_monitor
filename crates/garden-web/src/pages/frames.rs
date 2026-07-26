//! Camera frames: serving image bytes, and the gallery.

use crate::app::{AppState, Auth};
use crate::error::AppError;
use crate::pages::gardens::authorize;
use crate::ui;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::response::{IntoResponse, Response};
use axum::{Router, routing::get};
use garden_auth::Permission;
use garden_core::GardenId;
use garden_store::frames::Frame;
use maud::{Markup, html};
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/gardens/{id}/frames", get(gallery))
        .route("/gardens/{id}/frames/{frame}", get(single))
        .route("/gardens/{id}/frames/{frame}/image", get(image))
}

/// Serve the image bytes.
///
/// This route is the reason frames are not a static file mount. Every request goes
/// through the same membership check as the rest of the garden, because a photograph
/// of someone's kitchen is at least as sensitive as the sensor readings beside it.
async fn image(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path((id, frame_id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let garden: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    authorize(&state, &actor, garden, Permission::ViewGarden).await?;

    let frame_id = Uuid::parse_str(&frame_id).map_err(|_| AppError::NotFound)?;
    // Scoped to the garden in the query, so a frame id from one garden cannot be
    // fetched through another garden's URL even by someone who belongs to that one.
    let frame = state
        .store
        .find_frame(garden, frame_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let bytes = state.store.frame_bytes(&frame).await?;

    Ok((
        [
            (CONTENT_TYPE, frame.kind.content_type()),
            // The bytes were sniffed on the way in, but pin the type on the way out
            // too: a browser that sniffs its way to text/html would turn an uploaded
            // file into script running on our origin.
            (X_CONTENT_TYPE_OPTIONS, "nosniff"),
            // Frames never change, and `private` keeps them out of any shared cache
            // that might not know who asked.
            (CACHE_CONTROL, "private, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response())
}

async fn gallery(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
) -> Result<Markup, AppError> {
    let garden_id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    let (garden, _) = authorize(&state, &actor, garden_id, Permission::ViewGarden).await?;
    let now = state.now();

    let frames = state.store.recent_frames(garden_id, 60).await?;
    let total = state.store.frame_count(garden_id).await?;

    Ok(ui::page(
        &format!("Camera · {}", garden.name),
        Some(&actor),
        html! {
            div.row {
                div {
                    h1 { "Camera" }
                    p.muted.small style="margin:0" {
                        a href=(format!("/gardens/{garden_id}")) { (garden.name) }
                        " · " (total) " frame" @if total != 1 { "s" }
                    }
                }
            }

            @match frames.first() {
                None => div.card {
                    h3 { "No frames yet" }
                    p.muted.small style="margin:0" {
                        "The edge agent uploads a frame each time it captures one. \
                         A simulated garden renders its own."
                    }
                }
                Some(latest) => {
                    div.card {
                        img src=(latest.image_path()) alt="Latest camera frame"
                            style="width:100%; border-radius:8px; display:block";
                        (frame_meta(latest, now))
                    }
                }
            }

            @if frames.len() > 1 {
                h2 { "Recent" }
                div.slotgrid {
                    @for frame in frames.iter().skip(1) {
                        a href=(format!("/gardens/{garden_id}/frames/{}", frame.id))
                          style="text-decoration:none" {
                            img src=(frame.image_path()) alt="Camera frame"
                                loading="lazy"
                                style="width:100%; border-radius:6px; display:block";
                            span.small.muted {
                                (ui::relative(now.as_second() - frame.captured_at.as_second()))
                            }
                        }
                    }
                }
            }
        },
    ))
}

/// One frame, with links to its neighbours — a time-lapse you step through.
///
/// Prev/next links rather than a scrubber because the whole UI is deliberately
/// JavaScript-free; a range slider would be the first thing to break it.
async fn single(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path((id, frame_id)): Path<(String, String)>,
) -> Result<Markup, AppError> {
    let garden_id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    let (garden, _) = authorize(&state, &actor, garden_id, Permission::ViewGarden).await?;
    let now = state.now();

    let frame_id = Uuid::parse_str(&frame_id).map_err(|_| AppError::NotFound)?;
    let frame = state
        .store
        .find_frame(garden_id, frame_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let (older, newer) = state.store.frame_neighbours(garden_id, &frame).await?;

    Ok(ui::page(
        &format!("Frame · {}", garden.name),
        Some(&actor),
        html! {
            div.row {
                div {
                    h1 { "Frame" }
                    p.muted.small style="margin:0" {
                        a href=(format!("/gardens/{garden_id}/frames")) { "all frames" }
                        " · "
                        a href=(format!("/gardens/{garden_id}")) { (garden.name) }
                    }
                }
            }

            div.card {
                img src=(frame.image_path()) alt="Camera frame"
                    style="width:100%; border-radius:8px; display:block";
                (frame_meta(&frame, now))
            }

            div.row {
                @match &older {
                    Some(prev) => a.button href=(format!("/gardens/{garden_id}/frames/{}", prev.id)) { "← Earlier" },
                    None => span.muted.small { "Earliest frame" }
                }
                div.spacer {}
                @match &newer {
                    Some(next) => a.button href=(format!("/gardens/{garden_id}/frames/{}", next.id)) { "Later →" },
                    None => span.muted.small { "Latest frame" }
                }
            }
        },
    ))
}

fn frame_meta(frame: &Frame, now: jiff::Timestamp) -> Markup {
    html! {
        div.row style="margin-top:0.6rem" {
            span.small.muted {
                (ui::relative(now.as_second() - frame.captured_at.as_second()))
                " · " (frame.width) "×" (frame.height)
            }
            @if frame.source == garden_store::frames::FrameSource::Simulated {
                span.pill.sev-info { "simulated" }
            }
            @match frame.light_percent() {
                Some(percent) => {
                    @if frame.comparable {
                        // Captured in photo mode, so colour can be trusted against
                        // other frames rather than reflecting the time of day.
                        span.pill.health-up { "photo mode · " (format!("{percent:.0}%")) }
                    } @else {
                        span.pill.sev-advisory title="Captured under the ambient light curve, so colour is not comparable between frames" {
                            "ambient · " (format!("{percent:.0}%"))
                        }
                    }
                }
                None => span.pill.health-unknown { "light unknown" }
            }
        }
    }
}
