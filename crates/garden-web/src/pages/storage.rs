//! What a garden is using on disk, and how long to keep it.
//!
//! Camera frames are the only thing here that grows without bound, and the operator is
//! the only one who knows whether this particular garden's history is worth the space.
//! So the setting lives next to the number it controls.
//!
//! **Shortening the window is destructive and is treated as such.** The form does not
//! quietly delete two thousand photographs because someone typed 30 instead of 300; it
//! says exactly how many and how much, names the dates, and asks again.

use crate::app::{AppState, Auth};
use crate::error::AppError;
use crate::ui;
use axum::extract::{Form, Path, State};
use axum::response::Redirect;
use axum::{Router, routing::get, routing::post};
use garden_auth::Permission;
use garden_core::{Garden, GardenId};
use garden_store::settings::{
    FrameStorage, MAX_FRAME_RETENTION_DAYS, MIN_FRAME_RETENTION_DAYS,
};
use maud::{Markup, html};
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/gardens/{id}/storage", get(page))
        .route("/gardens/{id}/storage", post(update))
}

#[derive(Deserialize)]
pub struct RetentionForm {
    frame_retention_days: i64,
    /// Present only on the second submission, from the confirmation page.
    #[serde(default)]
    confirm: Option<String>,
}

async fn load(
    state: &AppState,
    actor: &garden_auth::Actor,
    id: &str,
) -> Result<Garden, AppError> {
    let garden: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    actor.require(garden, Permission::ConfigureGarden)?;
    state.store.find_garden(garden).await?.ok_or(AppError::NotFound)
}

async fn page(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
) -> Result<Markup, AppError> {
    let garden = load(&state, &actor, &id).await?;
    let held = state.store.frame_storage(garden.id).await?;
    let keep = state.store.frame_retention_days(garden.id).await?;
    let database = state.store.database_bytes().await?;

    Ok(ui::page(
        "Storage",
        Some(&actor),
        html! {
            p.muted.small { a href=(format!("/gardens/{}", garden.id)) { "← " (garden.name) } }
            h1 { "Storage" }

            div.grid {
                (stat("Photographs", &held.count.to_string(), oldest_note(&held)))
                (stat("Frames on disk", &format!("{:.0} MB", held.megabytes()),
                      "one file each, outside the database".into()))
                (stat("Database", &format!("{:.1} MB", database as f64 / 1_048_576.0),
                      "readings, tasks, measurements — this is what the nightly backup copies".into()))
            }

            form.card method="post" action=(format!("/gardens/{}/storage", garden.id)) {
                h2 style="margin-top:0" { "How long to keep photographs" }
                p.small.muted {
                    "Everything else is bounded — a season of canopy measurements is a few "
                    "thousand rows. Frames are one file an hour, so this is the only "
                    "setting that decides whether the disk fills."
                }
                label for="days" { "Days" }
                input #days type="number" name="frame_retention_days"
                      min=(MIN_FRAME_RETENTION_DAYS) max=(MAX_FRAME_RETENTION_DAYS)
                      value=(keep) style="max-width:8rem";
                p style="margin-top:0.75rem" { button.primary type="submit" { "Save" } }
            }

            p.small.muted {
                "Pruning runs once a day. Sensor readings are kept 90 days and canopy "
                "measurements 400; both are server-wide and set by "
                (code_span("GARDEN_RETAIN_READING_DAYS")) " and "
                (code_span("GARDEN_RETAIN_METRIC_DAYS")) "."
            }
        },
    ))
}

fn oldest_note(held: &FrameStorage) -> String {
    match held.oldest {
        Some(at) => format!("oldest {}", at.strftime("%-d %b %Y")),
        None => "nothing captured yet".into(),
    }
}

fn stat(label: &str, value: &str, note: String) -> Markup {
    html! {
        div.card {
            div.stat-label { (label) }
            div.stat { (value) }
            p.small.muted style="margin:0.35rem 0 0" { (note) }
        }
    }
}

fn code_span(text: &str) -> Markup {
    html! { code { (text) } }
}

async fn update(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
    Form(form): Form<RetentionForm>,
) -> Result<axum::response::Response, AppError> {
    use axum::response::IntoResponse;

    let garden = load(&state, &actor, &id).await?;
    let now = state.now();
    let wanted = form
        .frame_retention_days
        .clamp(MIN_FRAME_RETENTION_DAYS, MAX_FRAME_RETENTION_DAYS);

    // What this change would throw away. Asked before anything is written, so the
    // number on the confirmation is the real one rather than a guess.
    let doomed = state
        .store
        .frames_older_than(garden.id, wanted as f64, now)
        .await?;

    let confirmed = form.confirm.as_deref() == Some("yes");
    if doomed.is_empty() || confirmed {
        state
            .store
            .set_frame_retention_days(garden.id, wanted, now)
            .await?;
        if !doomed.is_empty() {
            let removed = state
                .store
                .prune_frames(garden.id, wanted as f64, now)
                .await?;
            state
                .store
                .log_event(
                    garden.id,
                    "storage.pruned",
                    Some(&format!("retention set to {wanted} days; {removed} frames deleted")),
                    Some(actor.id()),
                    now,
                )
                .await?;
            tracing::info!(garden = %garden.id, removed, wanted, "operator tightened retention");
        }
        return Ok(Redirect::to(&format!("/gardens/{}/storage", garden.id)).into_response());
    }

    Ok(confirmation(&garden, wanted, &doomed).into_response())
}

/// The page shown before anything is deleted.
fn confirmation(garden: &Garden, wanted: i64, doomed: &FrameStorage) -> Markup {
    let range = match (doomed.oldest, doomed.newest) {
        (Some(from), Some(to)) => format!(
            "taken between {} and {}",
            from.strftime("%-d %b %Y"),
            to.strftime("%-d %b %Y")
        ),
        _ => String::new(),
    };

    ui::plain_page(
        "Delete photographs?",
        html! {
            h1 { "Delete " (doomed.count) " photographs?" }
            p {
                "Keeping " (wanted) " days deletes "
                strong { (doomed.count) " photographs" }
                " (" (format!("{:.0} MB", doomed.megabytes())) ") from "
                strong { (garden.name) }
                @if !range.is_empty() { ", " (range) }
                "."
            }
            p.error { strong { "This cannot be undone." } }
            p.small.muted {
                "The measurements taken from those frames go with them — a canopy "
                "reading whose photograph is gone cannot be checked. Growth history "
                "before this date will have a hole in it."
            }

            form method="post" action=(format!("/gardens/{}/storage", garden.id)) {
                input type="hidden" name="frame_retention_days" value=(wanted);
                input type="hidden" name="confirm" value="yes";
                button.danger.primary type="submit" {
                    "Delete " (doomed.count) " photographs"
                }
            }
            p style="margin-top:0.75rem" {
                a.button href=(format!("/gardens/{}/storage", garden.id)) { "Cancel" }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use garden_store::settings::DEFAULT_FRAME_RETENTION_DAYS;

    #[test]
    fn the_form_cannot_offer_a_setting_that_would_empty_the_garden() {
        // Zero days would delete every photograph the moment it was saved. The floor
        // is a real number of days; turning the camera off is the agent's job. These
        // are the numbers the number field's min/max attributes carry.
        let bounds = [
            MIN_FRAME_RETENTION_DAYS,
            DEFAULT_FRAME_RETENTION_DAYS,
            MAX_FRAME_RETENTION_DAYS,
        ];
        assert!(bounds[0] >= 1, "the floor must keep something");
        assert!(bounds.windows(2).all(|w| w[0] < w[1]), "{bounds:?} out of order");
    }

    #[test]
    fn the_confirmation_states_the_count_the_size_and_the_dates() {
        // A warning that says "some photographs" is not a warning.
        let garden = Garden {
            id: GardenId::new(),
            name: "Kitchen".into(),
            model: garden_core::DeviceModel::Studio2,
            timezone: "UTC".into(),
            created_at: jiff::Timestamp::from_second(1_700_000_000).unwrap(),
        };
        let doomed = FrameStorage {
            count: 412,
            bytes: 1_288_490_188,
            oldest: Some(jiff::Timestamp::from_second(1_700_000_000).unwrap()),
            newest: Some(jiff::Timestamp::from_second(1_710_000_000).unwrap()),
        };

        let html = confirmation(&garden, 30, &doomed).into_string();
        assert!(html.contains("412"), "the count");
        assert!(html.contains("1229 MB"), "the size: {html}");
        assert!(html.contains("Nov 2023") && html.contains("Mar 2024"), "the dates");
        assert!(html.contains("cannot be undone"));
        assert!(html.contains("Kitchen"));
        // And a way out that is not the delete button.
        assert!(html.contains("Cancel"));
    }

    #[test]
    fn the_confirmation_carries_the_setting_forward_so_the_second_post_matches() {
        // If the hidden field drifted from what was previewed, the operator would
        // confirm one number and get another.
        let garden = Garden {
            id: GardenId::new(),
            name: "Kitchen".into(),
            model: garden_core::DeviceModel::Studio2,
            timezone: "UTC".into(),
            created_at: jiff::Timestamp::from_second(1_700_000_000).unwrap(),
        };
        let html = confirmation(&garden, 45, &FrameStorage { count: 3, bytes: 900, ..Default::default() })
            .into_string();
        assert!(html.contains(r#"name="frame_retention_days" value="45""#), "{html}");
        assert!(html.contains(r#"name="confirm" value="yes""#));
    }
}

#[cfg(test)]
mod flow {
    //! The destructive path, end to end through the handler.

    use super::*;
    use crate::app::Config;
    use garden_auth::{Actor, EmailAddress};
    use garden_core::{DeviceModel, Timestamp, time::add_days};
    use garden_store::Store;
    use garden_store::frames::{FrameSource, NewFrame};

    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
        0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
        0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
        0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
        0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn now() -> Timestamp {
        Timestamp::from_second(1_800_000_000).unwrap()
    }

    async fn fixture() -> (AppState, Actor, Garden, garden_auth::UserId) {
        let store = Store::in_memory().await.unwrap();
        let user = store
            .create_user(
                EmailAddress::parse("phil@example.com").unwrap(),
                "Phil",
                "a long enough password",
                now(),
            )
            .await
            .unwrap();
        let garden = store
            .create_garden("Kitchen", DeviceModel::Studio2, "UTC", user.id, now())
            .await
            .unwrap();
        let memberships = store.memberships_of_user(user.id).await.unwrap();
        let actor = Actor::new(user, memberships);

        // Ten daily frames, the oldest 120 days back.
        for days in [120.0, 110.0, 100.0, 95.0, 80.0, 60.0, 40.0, 20.0, 5.0, 1.0] {
            store
                .put_frame(NewFrame {
                    garden: garden.id,
                    captured_at: add_days(now(), -days),
                    width: 1,
                    height: 1,
                    light_duty_milli: Some(800),
                    comparable: true,
                    source: FrameSource::Agent,
                    bytes: PNG,
                })
                .await
                .unwrap()
                .unwrap();
        }

        let state = AppState::new(
            store,
            Config {
                secure_cookies: false,
                base_url: "http://localhost:8080".into(),
                agent_token: None,
            },
        )
        .with_clock_at(now());
        let owner = actor.id();
        (state, actor, garden, owner)
    }

    async fn post(
        state: &AppState,
        actor: &Actor,
        garden: &Garden,
        days: i64,
        confirm: bool,
    ) -> String {
        use axum::body::to_bytes;
        let response = update(
            State(state.clone()),
            Auth(actor.clone()),
            Path(garden.id.to_string()),
            Form(RetentionForm {
                frame_retention_days: days,
                confirm: confirm.then(|| "yes".to_string()),
            }),
        )
        .await
        .expect("handler");
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        String::from_utf8_lossy(&body).to_string()
    }

    #[tokio::test]
    async fn shortening_the_window_asks_before_it_deletes() {
        let (state, actor, garden, _) = fixture().await;
        let before = state.store.frame_storage(garden.id).await.unwrap().count;

        let body = post(&state, &actor, &garden, 30, false).await;
        assert!(body.contains("cannot be undone"), "expected a confirmation");
        assert!(body.contains("photographs"));

        // Nothing has gone, and the setting has not moved either.
        assert_eq!(state.store.frame_storage(garden.id).await.unwrap().count, before);
        assert_eq!(state.store.frame_retention_days(garden.id).await.unwrap(), 90);
    }

    #[tokio::test]
    async fn confirming_deletes_and_saves() {
        let (state, actor, garden, _) = fixture().await;
        post(&state, &actor, &garden, 30, true).await;

        // 120, 110, 100, 95, 80, 60, 40 days back are all older than 30.
        assert_eq!(state.store.frame_storage(garden.id).await.unwrap().count, 3);
        assert_eq!(state.store.frame_retention_days(garden.id).await.unwrap(), 30);
    }

    #[tokio::test]
    async fn lengthening_the_window_needs_no_confirmation() {
        // Nothing is at risk, so an extra click would be theatre.
        let (state, actor, garden, _) = fixture().await;
        let before = state.store.frame_storage(garden.id).await.unwrap().count;

        let body = post(&state, &actor, &garden, 365, false).await;
        assert!(!body.contains("cannot be undone"), "should have just saved");
        assert_eq!(state.store.frame_retention_days(garden.id).await.unwrap(), 365);
        assert_eq!(state.store.frame_storage(garden.id).await.unwrap().count, before);
    }

    #[tokio::test]
    async fn the_deletion_is_recorded_in_the_garden_log() {
        // Frames vanishing should be traceable to a person and a decision.
        let (state, actor, garden, _) = fixture().await;
        post(&state, &actor, &garden, 30, true).await;

        let events = state.store.recent_events(garden.id, 10).await.unwrap();
        assert!(
            events.iter().any(|e| e.kind == "storage.pruned"),
            "expected a storage.pruned event, got {:?}",
            events.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn a_member_without_configure_cannot_reach_the_page() {
        let (state, _, garden, owner) = fixture().await;
        let viewer = state
            .store
            .create_user(
                EmailAddress::parse("viewer@example.com").unwrap(),
                "Viewer",
                "a long enough password",
                now(),
            )
            .await
            .unwrap();
        state
            .store
            .grant_membership(&garden_auth::Membership::granted(
                garden.id,
                viewer.id,
                garden_auth::Role::Caretaker,
                owner,
                now(),
            ))
            .await
            .unwrap();
        let memberships = state.store.memberships_of_user(viewer.id).await.unwrap();
        let actor = Actor::new(viewer, memberships);

        assert!(load(&state, &actor, &garden.id.to_string()).await.is_err());
    }
}
