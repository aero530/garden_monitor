//! The light and pump programme the Pi runs.
//!
//! This page does not control anything directly, and that is the design rather than a
//! limitation. The brain is never in the control loop: it hands the Pi a schedule, the
//! Pi runs it from its own clock, and if this whole VM disappears the garden carries on
//! unchanged. What is set here reaches the agent on its next telemetry call.
//!
//! Which also means it does nothing at all unless an agent is running with
//! `--own-actuators`. Saying so plainly matters more than it sounds — otherwise the
//! obvious reading of a schedule form is that the lights just changed.

use crate::app::{AppState, Auth};
use crate::error::AppError;
use crate::ui;
use axum::extract::{Form, Path, State};
use axum::response::Redirect;
use axum::{Router, routing::get, routing::post};
use garden_auth::Permission;
use garden_core::{Garden, GardenId};
use garden_hal::Schedule;
use maud::{Markup, html};
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/gardens/{id}/schedule", get(page))
        .route("/gardens/{id}/schedule", post(update))
}

#[derive(Deserialize)]
pub struct ScheduleForm {
    light_start_hour: u8,
    light_hours: f32,
    light_duty_percent: f32,
    ramp_minutes: f32,
    pump_on_minutes: f32,
    pump_cycle_minutes: f32,
    pump_duty_percent: f32,
}

impl ScheduleForm {
    fn into_schedule(self) -> Schedule {
        Schedule {
            light_start_hour: self.light_start_hour,
            light_hours: self.light_hours,
            // Percentages in the form, fractions in the type. Asking someone to type
            // 0.85 for a brightness is a needless translation.
            light_duty: self.light_duty_percent / 100.0,
            ramp_minutes: self.ramp_minutes,
            pump_on_minutes: self.pump_on_minutes,
            pump_cycle_minutes: self.pump_cycle_minutes,
            pump_duty: self.pump_duty_percent / 100.0,
        }
    }
}

async fn load(state: &AppState, actor: &garden_auth::Actor, id: &str) -> Result<Garden, AppError> {
    let garden: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    actor.require(garden, Permission::ControlHardware)?;
    state.store.find_garden(garden).await?.ok_or(AppError::NotFound)
}

async fn page(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
) -> Result<Markup, AppError> {
    let garden = load(&state, &actor, &id).await?;
    let stored = state.store.schedule(garden.id).await?;
    render(&actor, &garden, stored, None)
}

fn render(
    actor: &garden_auth::Actor,
    garden: &Garden,
    stored: Option<Schedule>,
    error: Option<String>,
) -> Result<Markup, AppError> {
    // With nothing stored the agent runs its own default, so that is what the form
    // should show — an empty form would imply the garden is dark.
    let current = stored.unwrap_or(Schedule::DEFAULT);

    Ok(ui::page(
        "Schedule",
        Some(actor),
        html! {
            p.muted.small { a href=(format!("/gardens/{}", garden.id)) { "← " (garden.name) } }
            h1 { "Light and pump schedule" }

            @if stored.is_none() {
                p.muted.small {
                    "Nothing has been set, so the agent runs its own default. Saving here "
                    "sends this garden a schedule of its own."
                }
            }

            div.card {
                p.small style="margin:0" {
                    strong { "This only does something if the agent owns the actuators." }
                    " Until firmware takeover the factory firmware drives the lights and "
                    "pump, and " code { "garden-edge run" } " ignores this unless it was "
                    "started with " code { "--own-actuators" } ". The schedule reaches the "
                    "Pi on its next telemetry call and then runs from the Pi's own clock — "
                    "if this server goes away, the garden carries on."
                }
            }

            @if let Some(message) = &error {
                p.error { (message) }
            }

            form.card method="post" action=(format!("/gardens/{}/schedule", garden.id)) {
                h2 style="margin-top:0" { "Light" }
                div.row {
                    (number("light_start_hour", "Starts at (hour)", f64::from(current.light_start_hour), 0.0, 23.0, 1.0))
                    (number("light_hours", "Hours per day", f64::from(current.light_hours), 0.0, 24.0, 0.5))
                    (number("light_duty_percent", "Brightness %", f64::from(current.light_duty * 100.0), 0.0, 100.0, 1.0))
                    (number("ramp_minutes", "Dawn/dusk ramp (min)", f64::from(current.ramp_minutes), 0.0, 240.0, 5.0))
                }
                p.small.muted {
                    "The ramp is what the stock firmware does, and what "
                    code { "garden-edge watch-pwm" } " records during parity capture. A step "
                    "change is a current spike every morning and a shock to plants that "
                    "have adapted to a gradual dawn."
                }

                h2 { "Pump" }
                div.row {
                    (number("pump_on_minutes", "Runs for (min)", f64::from(current.pump_on_minutes), 0.0, 60.0, 1.0))
                    (number("pump_cycle_minutes", "Every (min)", f64::from(current.pump_cycle_minutes), 1.0, 240.0, 5.0))
                    (number("pump_duty_percent", "Power %", f64::from(current.pump_duty * 100.0), 0.0, 30.0, 1.0))
                }
                p.small.muted {
                    "Power is capped at 30% whatever is entered — full output is believed to "
                    "exceed the supply's budget, and after takeover there is no vendor "
                    "firmware left to catch the mistake. The pump runs through the dark "
                    "hours too: roots do not stop needing water when the lights go off."
                }

                p style="margin-top:1rem" {
                    button.primary type="submit" { "Save schedule" }
                    " "
                    span.muted.small {
                        "currently " (format!("{:.1}", current.daily_duty_hours()))
                        " duty-hours of light a day"
                    }
                }
            }

            h2 { "The day this produces" }
            p.muted.small {
                "Every hour, from the Pi's local midnight. This is the same table "
                code { "garden-cli schedule preview" } " prints."
            }
            (preview(&current))
        },
    ))
}

fn number(name: &str, label: &str, value: f64, min: f64, max: f64, step: f64) -> Markup {
    html! {
        div style="flex:1; min-width:9rem" {
            label for=(name) { (label) }
            input #(name) type="number" name=(name) value=(format!("{value}"))
                  min=(format!("{min}")) max=(format!("{max}")) step=(format!("{step}"))
                  required;
        }
    }
}

/// The whole day as a table, because a schedule is easier to check than to read.
fn preview(schedule: &Schedule) -> Markup {
    html! {
        div.table-wrap {
            table {
                thead { tr { th { "Hour" } th { "Light" } th { "Pump" } } }
                tbody {
                    @for hour in 0..24u32 {
                        @let point: garden_hal::Setpoint = schedule.setpoint(hour * 3600);
                        tr {
                            td { (format!("{hour:02}:00")) }
                            td { (bar(point.light.get())) }
                            td { (bar(point.pump.get())) }
                        }
                    }
                }
            }
        }
    }
}

/// A duty as a proportional bar. Reads faster than a column of percentages when what
/// you are checking is the shape of the day.
fn bar(duty: f32) -> Markup {
    let percent = (duty * 100.0).round();
    html! {
        div.row style="gap:0.5rem" {
            div style="flex:0 0 6rem; background:var(--line); border-radius:3px; height:0.55rem" {
                div style=(format!(
                    "width:{percent}%; background:var(--accent); border-radius:3px; height:100%"
                )) {}
            }
            span.small.muted { (format!("{percent:.0}%")) }
        }
    }
}

async fn update(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
    Form(form): Form<ScheduleForm>,
) -> Result<axum::response::Response, AppError> {
    use axum::response::IntoResponse;

    let garden = load(&state, &actor, &id).await?;
    let wanted = form.into_schedule();

    // Refused rather than clamped, and refused here where someone can see the error.
    // A schedule silently adjusted into something else is worse than one rejected: the
    // operator would believe the garden is running what they typed.
    if let Err(error) = wanted.validate() {
        return Ok(render(&actor, &garden, Some(wanted), Some(error.to_string()))?.into_response());
    }

    let now = state.now();
    state.store.set_schedule(garden.id, &wanted, now).await?;
    state
        .store
        .log_event(
            garden.id,
            "schedule.set",
            Some(&format!(
                "{:.1} h light at {:.0}%, pump {:.0}/{:.0} min at {:.0}%",
                wanted.light_hours,
                wanted.light_duty * 100.0,
                wanted.pump_on_minutes,
                wanted.pump_cycle_minutes,
                wanted.pump_duty * 100.0
            )),
            Some(actor.id()),
            now,
        )
        .await?;

    Ok(Redirect::to(&format!("/gardens/{}/schedule", garden.id)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(duty_percent: f32, pump_percent: f32) -> ScheduleForm {
        ScheduleForm {
            light_start_hour: 6,
            light_hours: 16.0,
            light_duty_percent: duty_percent,
            ramp_minutes: 30.0,
            pump_on_minutes: 15.0,
            pump_cycle_minutes: 60.0,
            pump_duty_percent: pump_percent,
        }
    }

    #[test]
    fn percentages_in_the_form_become_fractions_in_the_type() {
        // Asking somebody to type 0.85 for a brightness is a needless translation, and
        // getting the conversion wrong would be a factor of a hundred on a real pin.
        let s = form(85.0, 25.0).into_schedule();
        assert!((s.light_duty - 0.85).abs() < 1e-6);
        assert!((s.pump_duty - 0.25).abs() < 1e-6);
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn a_pump_setting_above_the_ceiling_is_refused_not_quietly_reduced() {
        // Clamping would leave the operator believing the garden runs what they typed.
        let s = form(85.0, 90.0).into_schedule();
        assert!(s.validate().is_err());
    }

    #[test]
    fn the_preview_covers_every_hour_of_the_day() {
        let html = preview(&Schedule::DEFAULT).into_string();
        for hour in 0..24 {
            assert!(html.contains(&format!("{hour:02}:00")), "missing hour {hour}");
        }
    }

    #[test]
    fn the_preview_shows_the_lights_off_overnight_and_the_pump_still_running() {
        // The single most surprising thing about the default programme, and the thing
        // an operator should be able to confirm at a glance rather than trust.
        let s = Schedule::DEFAULT;
        let night = s.setpoint(3 * 3600);
        assert!(night.light.is_off());
        assert!(!night.pump.is_off());
    }

    #[test]
    fn a_garden_with_no_stored_schedule_shows_the_agents_default() {
        // An empty form would imply the garden is dark, which is the opposite of true.
        let actor = test_actor();
        let garden = test_garden();
        let html = render(&actor, &garden, None, None).unwrap().into_string();
        assert!(html.contains("runs its own default"));
        assert!(html.contains(r#"name="light_hours" value="16""#), "{html}");
    }

    #[test]
    fn the_page_says_plainly_that_it_may_do_nothing() {
        // Without this, the obvious reading of a schedule form is that the lights just
        // changed. They have not, unless the agent owns them.
        let html = render(&test_actor(), &test_garden(), None, None)
            .unwrap()
            .into_string();
        assert!(html.contains("--own-actuators"));
        assert!(html.contains("only does something"));
    }

    #[test]
    fn a_refused_schedule_comes_back_with_the_values_still_in_the_form() {
        // Bouncing to an empty form and making somebody retype seven fields to find
        // out which one was wrong is its own small cruelty.
        let bad = form(85.0, 90.0).into_schedule();
        let html = render(&test_actor(), &test_garden(), Some(bad), Some("nope".into()))
            .unwrap()
            .into_string();
        assert!(html.contains("nope"));
        assert!(html.contains(r#"name="pump_duty_percent" value="90""#), "{html}");
    }

    fn test_garden() -> Garden {
        Garden {
            id: GardenId::new(),
            name: "Kitchen".into(),
            model: garden_core::DeviceModel::Studio2,
            timezone: "UTC".into(),
            created_at: jiff::Timestamp::from_second(1_700_000_000).unwrap(),
        }
    }

    fn test_actor() -> garden_auth::Actor {
        let user = garden_auth::User::new(
            garden_auth::EmailAddress::parse("phil@example.com").unwrap(),
            "Phil",
            jiff::Timestamp::from_second(1_700_000_000).unwrap(),
        );
        garden_auth::Actor::new(user, [])
    }
}
