//! Recording what is planted where, and what has been done to it.

use crate::app::{AppState, Auth};
use crate::error::AppError;
use crate::pages::gardens::authorize;
use crate::{state as garden_state, ui};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Form, Router, routing::get, routing::post};
use gardyn_auth::Permission;
use gardyn_core::{
    GardenId, GardenState, LightZone, Planting, PlantingId, SlotId, Variety, VarietyBook,
    VarietyId,
};
use gardyn_store::plantings::{PlantingError, PlantingEvent};
use jiff::Timestamp;
use maud::{Markup, html};
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/gardens/{id}/slots", get(page))
        .route("/gardens/{id}/slots/{slot}/plant", post(plant))
        .route("/gardens/{id}/plantings/{planting}/log/{event}", post(log_event))
        .route("/gardens/{id}/plantings/{planting}/remove", post(remove))
}

/// Interpret a `<input type="date">` value as midnight UTC.
///
/// Good enough for a planting date — nobody records the minute they pushed a cube in,
/// and the rules reason in days.
fn parse_date(value: &str, fallback: Timestamp) -> Timestamp {
    format!("{}T00:00:00Z", value.trim())
        .parse()
        .unwrap_or(fallback)
}

async fn page(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
) -> Result<Markup, AppError> {
    let id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    let (garden, role) = authorize(&state, &actor, id, Permission::ViewGarden).await?;
    let now = state.now();

    let snapshot = garden_state::build(&state.store, &garden, now).await?;
    let history = state.store.planting_history(id, 20).await?;
    let book = VarietyBook::starter();
    let can_edit = role.grants(Permission::ManagePlantings);
    let today = now.to_string().split('T').next().unwrap_or_default().to_string();

    Ok(ui::page(
        &format!("Slots · {}", garden.name),
        Some(&actor),
        html! {
            div.row {
                div {
                    h1 { "Slots" }
                    p.muted.small style="margin:0" {
                        a href=(format!("/gardens/{id}")) { (garden.name) }
                        " · " (snapshot.occupied_slots()) " of "
                        (snapshot.geometry.slot_count()) " planted"
                    }
                }
            }

            @if !can_edit {
                p.muted.small { "You can see what is growing here, but not change it." }
            }

            p.muted.small {
                "Laid out as the tower is: " (snapshot.geometry.columns)
                " column" @if snapshot.geometry.columns != 1 { "s" }
                " of " (snapshot.geometry.rows_per_column) ", top to bottom. The bar beside "
                "each slot is its light zone — brightest in the middle, where Gardyn "
                "says fruiting plants belong."
            }

            div.tower style=(format!(
                "grid-template-columns: repeat({}, minmax(0, 1fr))",
                snapshot.geometry.columns.max(1)
            )) {
                @for column in 0..snapshot.geometry.columns {
                    div.tower-column {
                        div.tower-head { "column " (column + 1) }
                        @for slot in snapshot.geometry.column(column) {
                            @let zone = snapshot.geometry.light_zone(slot);
                            div.slot-row {
                                div.zone-strip class=(format!("zone-{}", zone.slug()))
                                    title=(zone.label()) {}
                                @match snapshot.planting_in(slot) {
                                    Some(planting) => {
                                        (occupied(&snapshot, planting, id, zone, can_edit, now))
                                    }
                                    None => (empty(slot, id, zone, &book, can_edit, &today)),
                                }
                            }
                        }
                    }
                }
            }

            @if !history.is_empty() {
                h2 { "Previously grown" }
                div.table-wrap {
                    table {
                        thead {
                            tr { th { "Slot" } th { "Variety" } th { "Harvests" } th { "Pulled" } }
                        }
                        tbody {
                            @for planting in &history {
                                tr {
                                    td.small { (planting.slot.0 + 1) }
                                    td.small {
                                        (book.get(&planting.variety)
                                            .map(|v| v.name.clone())
                                            .unwrap_or_else(|| planting.variety.0.clone()))
                                    }
                                    td.small { (planting.harvest_count) }
                                    td.small.muted {
                                        @match planting.removed_at {
                                            Some(at) => (ui::relative(now.as_second() - at.as_second())),
                                            None => "—",
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
    ))
}

fn occupied(
    snapshot: &GardenState,
    planting: &Planting,
    garden: GardenId,
    zone: LightZone,
    can_edit: bool,
    now: Timestamp,
) -> Markup {
    let variety = snapshot.variety_of(planting);
    let pid = planting.id.0;
    // Gardyn's guide is explicit that a high-light plant in a dim slot will sulk. The
    // reverse is fine, so this only complains in one direction.
    let misplaced = variety.filter(|v| !v.light_zone.satisfied_by(zone));

    html! {
        div.card {
            div.row {
                strong { (planting.slot.to_string()) }
                div.spacer {}
                @if let Some(v) = variety {
                    span.pill.sev-info { (planting.stage(v, now).label()) }
                }
            }
            @if let Some(v) = misplaced {
                p.small style="margin:0.3rem 0 0" {
                    span.pill.sev-advisory {
                        "wants " (v.light_zone.label())
                    }
                    span.muted { " · this slot is " (zone.label()) }
                }
            }
            h3 style="margin:0.4rem 0 0.1rem" {
                @match variety {
                    Some(v) => (v.name),
                    // A planting whose variety left the book still shows something
                    // rather than vanishing from the grid.
                    None => (planting.variety.0),
                }
            }
            p.small.muted style="margin:0" {
                (format!("{:.0} days old", planting.age_days(now)))
                @if planting.germinated_at.is_none() { " · not sprouted yet" }
                @if planting.harvest_count > 0 {
                    " · " (planting.harvest_count) " harvest"
                    @if planting.harvest_count != 1 { "s" }
                }
            }

            @if let Some(v) = variety {
                (next_up(snapshot, planting, v, now))
            }

            @if can_edit {
                div.row style="margin-top:0.6rem; gap:0.3rem" {
                    @if planting.germinated_at.is_none() {
                        (log_button(garden, pid, PlantingEvent::Germinated, "Sprouted", true))
                    } @else {
                        (log_button(garden, pid, PlantingEvent::Harvested, "Harvested", true))
                        (log_button(garden, pid, PlantingEvent::RootsChecked, "Roots", false))
                        (log_button(garden, pid, PlantingEvent::Pruned, "Pruned", false))
                        @if planting.thinned_at.is_none() {
                            (log_button(garden, pid, PlantingEvent::Thinned, "Thinned", false))
                        }
                    }
                    form method="post"
                         action=(format!("/gardens/{garden}/plantings/{pid}/remove"))
                         onsubmit="return confirm('Pull this plant? The slot frees up and its history is kept.')" {
                        button.link.danger type="submit" { "Pull" }
                    }
                }
            }
        }
    }
}

/// The single most useful line on the card: what happens next, and when.
///
/// When the camera is measuring a slot, this defers to the measurement exactly as the
/// harvest rule does. Otherwise the card would announce "ready to harvest" from the
/// calendar for a plant the system has decided is still too small to pick, and the
/// operator would be looking at two parts of the same app disagreeing.
fn next_up(
    snapshot: &GardenState,
    planting: &Planting,
    variety: &Variety,
    now: Timestamp,
) -> Markup {
    let stage = planting.stage(variety, now);
    let undersized = snapshot
        .metrics_for(planting.slot)
        .zip(variety.harvest_canopy_cm2)
        .map(|(metrics, threshold)| (metrics.canopy_area_cm2, threshold))
        .filter(|(area, threshold)| area < threshold);

    html! {
        p.small style="margin:0.35rem 0 0" {
            @match planting.days_until_harvest(variety, now) {
                Some(days) if days > 0.5 => {
                    span.muted { "harvest in " } (format!("{days:.0} days"))
                }
                Some(_) if stage.is_producing() => {
                    @match undersized {
                        Some((area, threshold)) => {
                            span.muted {
                                "due by the book, but only " (format!("{area:.0}"))
                                " of " (format!("{threshold:.0}")) " cm² — still sizing up"
                            }
                        }
                        None => span.pill.sev-advisory { "ready to harvest" }
                    }
                }
                _ => {
                    @if planting.germinated_at.is_none() {
                        span.muted {
                            "expect a sprout around day " (variety.germination_days)
                        }
                    } @else {
                        span.muted { "growing on" }
                    }
                }
            }
        }
    }
}

fn log_button(garden: GardenId, planting: u64, event: PlantingEvent, label: &str, primary: bool) -> Markup {
    html! {
        form method="post"
             action=(format!("/gardens/{garden}/plantings/{planting}/log/{}", event.slug()))
             style="display:inline" {
            @if primary {
                button.primary type="submit" { (label) }
            } @else {
                button type="submit" { (label) }
            }
        }
    }
}

fn empty(
    slot: SlotId,
    garden: GardenId,
    zone: LightZone,
    book: &VarietyBook,
    can_edit: bool,
    today: &str,
) -> Markup {
    html! {
        div.card style="border-style:dashed" {
            div.row {
                strong.muted { (slot.to_string()) }
                div.spacer {}
                span.muted.small { (zone.label()) }
            }
            @if can_edit {
                form method="post" action=(format!("/gardens/{garden}/slots/{}/plant", slot.0)) {
                    label for=(format!("variety-{}", slot.0)) { "Plant" }
                    // Varieties this slot can actually support come first, so the
                    // obvious choice is also the correct one.
                    select id=(format!("variety-{}", slot.0)) name="variety" {
                        optgroup label=(format!("Suits {}", zone.label())) {
                            @for variety in book.iter().filter(|v| v.light_zone.satisfied_by(zone)) {
                                option value=(variety.id.0) { (variety.name) }
                            }
                        }
                        optgroup label="Needs more light than this slot has" {
                            @for variety in book.iter().filter(|v| !v.light_zone.satisfied_by(zone)) {
                                option value=(variety.id.0) { (variety.name) }
                            }
                        }
                    }
                    label for=(format!("date-{}", slot.0)) { "Date" }
                    input id=(format!("date-{}", slot.0)) type="date" name="planted_on" value=(today);
                    p style="margin:0.6rem 0 0" {
                        button.primary type="submit" style="width:100%" { "Plant" }
                    }
                }
            }
        }
    }
}

#[derive(Deserialize)]
pub struct PlantForm {
    variety: String,
    planted_on: Option<String>,
}

async fn plant(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path((id, slot)): Path<(String, u8)>,
    Form(form): Form<PlantForm>,
) -> Result<Response, AppError> {
    let id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    let (garden, _) = authorize(&state, &actor, id, Permission::ManagePlantings).await?;
    let now = state.now();

    // The variety has to exist in the book, or the rules would have nothing to reason
    // with and the slot would render as "unknown variety" forever.
    let book = VarietyBook::starter();
    let variety = VarietyId::new(form.variety.trim());
    let Some(known) = book.get(&variety) else {
        return Err(AppError::bad_request("That is not a variety we know about."));
    };

    let planted_at = form
        .planted_on
        .as_deref()
        .map(|d| parse_date(d, now))
        .unwrap_or(now);

    let outcome = state
        .store
        .plant(
            id,
            SlotId(slot),
            &variety,
            planted_at,
            garden.model.slot_count(),
            Some(actor.id()),
        )
        .await?;

    match outcome {
        Ok(planting) => {
            state
                .store
                .log_event(
                    id,
                    "planting.added",
                    Some(&format!("{} planted in {}", known.name, planting.slot)),
                    Some(actor.id()),
                    now,
                )
                .await?;
        }
        Err(PlantingError::SlotOccupied) => {
            return Err(AppError::bad_request(
                "Something is already growing in that slot — pull it first.",
            ));
        }
        Err(PlantingError::NoSuchSlot) => return Err(AppError::NotFound),
    }

    Ok(Redirect::to(&format!("/gardens/{id}/slots")).into_response())
}

async fn log_event(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path((id, planting, event)): Path<(String, u64, String)>,
) -> Result<Response, AppError> {
    let id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    authorize(&state, &actor, id, Permission::ManagePlantings).await?;
    let now = state.now();

    let event = PlantingEvent::parse(&event).ok_or(AppError::NotFound)?;
    let planting_id = PlantingId(planting);
    let existing = state
        .store
        .find_planting(id, planting_id)
        .await?
        .ok_or(AppError::NotFound)?;

    state
        .store
        .record_planting_event(id, planting_id, event, now)
        .await?;

    let book = VarietyBook::starter();
    let name = book
        .get(&existing.variety)
        .map(|v| v.name.clone())
        .unwrap_or_else(|| existing.variety.0.clone());
    state
        .store
        .log_event(
            id,
            &format!("planting.{}", event.slug()),
            Some(&format!("{name} in {} — {}", existing.slot, event.label())),
            Some(actor.id()),
            now,
        )
        .await?;

    Ok(Redirect::to(&format!("/gardens/{id}/slots")).into_response())
}

async fn remove(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path((id, planting)): Path<(String, u64)>,
) -> Result<Response, AppError> {
    let id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    authorize(&state, &actor, id, Permission::ManagePlantings).await?;
    let now = state.now();

    let planting_id = PlantingId(planting);
    let existing = state
        .store
        .find_planting(id, planting_id)
        .await?
        .ok_or(AppError::NotFound)?;

    state.store.remove_planting(id, planting_id, now).await?;
    state
        .store
        .log_event(
            id,
            "planting.removed",
            Some(&format!("pulled from {}", existing.slot)),
            Some(actor.id()),
            now,
        )
        .await?;

    Ok(Redirect::to(&format!("/gardens/{id}/slots")).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    #[test]
    fn a_date_field_parses_to_midnight_utc() {
        let parsed = parse_date("2026-03-14", t0());
        assert_eq!(parsed.to_string(), "2026-03-14T00:00:00Z");
    }

    #[test]
    fn a_junk_date_falls_back_rather_than_failing_the_request() {
        // Planting something is more important than getting the date exactly right.
        assert_eq!(parse_date("not-a-date", t0()), t0());
        assert_eq!(parse_date("", t0()), t0());
        assert_eq!(parse_date("2026-13-45", t0()), t0());
    }
}
