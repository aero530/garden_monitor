//! The garden list and the per-garden dashboard.

use crate::app::{AppState, Auth};
use crate::error::AppError;
use crate::{demo, ui};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Form, Router, routing::get, routing::post};
use garden_auth::{Actor, Permission, Role};
use garden_core::{DeviceModel, Garden, GardenId, GardenState};
use garden_store::gardens::GardenListing;
use garden_store::tasks::TaskRecord;
use maud::{Markup, html};
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/gardens/new", get(new_form))
        .route("/gardens", post(create))
        .route("/gardens/{id}", get(detail))
        .route("/gardens/{id}/settings", post(update))
        .route("/gardens/{id}/delete", post(delete))
}

/// Resolve a garden the caller is allowed to touch.
///
/// Every garden route goes through here, so there is exactly one place where a
/// membership is checked — and one place to get it wrong.
pub async fn authorize(
    state: &AppState,
    actor: &Actor,
    id: GardenId,
    permission: Permission,
) -> Result<(Garden, Role), AppError> {
    let role = actor.require(id, permission)?;
    let garden = state
        .store
        .find_garden(id)
        .await?
        // A membership pointing at a missing garden should be impossible thanks to
        // the foreign key, but conceal it the same way rather than leaking a 500.
        .ok_or(AppError::Denied(garden_auth::AccessDenied::NotAMember {
            garden: id,
        }))?;
    Ok((garden, role))
}

async fn list(State(state): State<AppState>, Auth(actor): Auth) -> Result<Markup, AppError> {
    let listings = state.store.gardens_for_user(actor.id()).await?;
    let (mine, shared): (Vec<_>, Vec<_>) =
        listings.iter().partition(|l| l.role == Role::Owner);

    Ok(ui::page(
        "Gardens",
        Some(&actor),
        html! {
            div.row {
                div { h1 { "Your gardens" } }
                div.spacer {}
                a.button.primary href="/gardens/new" { "Add a garden" }
            }

            @if listings.is_empty() {
                div.card {
                    p { "No gardens yet." }
                    p.muted.small {
                        "Add one to get started. If someone has shared theirs with you, \
                         open the invitation link they sent."
                    }
                }
            }

            @if !mine.is_empty() {
                h2 { "Yours" }
                div.grid { @for listing in &mine { (garden_card(listing)) } }
            }

            @if !shared.is_empty() {
                h2 { "Shared with you" }
                div.grid { @for listing in &shared { (garden_card(listing)) } }
            }
        },
    ))
}

fn garden_card(listing: &GardenListing) -> Markup {
    html! {
        a.card href=(format!("/gardens/{}", listing.garden.id))
          style="text-decoration:none; color:inherit; display:block" {
            h3 { (listing.garden.name) }
            p.muted.small style="margin:0" { (listing.garden.model.label()) }
            div.row style="margin-top:0.6rem" {
                span.pill class=(format!("sev-{}", role_tone(listing.role))) { (listing.role.label()) }
                @if listing.is_shared() {
                    span.small.muted {
                        (listing.member_count) " people"
                    }
                }
            }
        }
    }
}

fn role_tone(role: Role) -> &'static str {
    match role {
        Role::Owner => "advisory",
        Role::Steward => "important",
        Role::Caretaker => "info",
        Role::Viewer => "info",
    }
}

/// How far back a simulated garden is dated, so it opens with a season behind it.
const SIMULATED_GARDEN_AGE_DAYS: f64 = 70.0;

#[derive(Deserialize)]
pub struct NewGardenForm {
    name: String,
    model: String,
    timezone: Option<String>,
}

async fn new_form(Auth(actor): Auth) -> Markup {
    ui::page(
        "Add a garden",
        Some(&actor),
        html! {
            h1 { "Add a garden" }
            form method="post" action="/gardens" style="max-width:26rem" {
                label for="name" { "Name" }
                input #name type="text" name="name" required placeholder="Kitchen" autofocus;
                p.small.muted { "What you call it. Two gardens on one account need telling apart." }

                label for="model" { "Model" }
                select #model name="model" {
                    option value="studio2" { "Gardyn Studio 2" }
                    option value="studio1" { "Gardyn Studio" }
                    option value="home4" { "Gardyn Home 4" }
                    option value="home3" { "Gardyn Home 3" }
                    option value="simulated" { "Simulated (try it without hardware)" }
                }
                p.small.muted {
                    "A simulated garden runs against the physics model, so you can see \
                     the dashboard and rules working before any hardware is wired up."
                }

                label for="timezone" { "Timezone" }
                input #timezone type="text" name="timezone" value="UTC"
                      placeholder="America/Denver";
                p.small.muted { "Used for quiet hours and the daily brief." }

                p style="margin-top:1rem" { button.primary type="submit" { "Add garden" } }
            }
        },
    )
}

async fn create(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Form(form): Form<NewGardenForm>,
) -> Result<Response, AppError> {
    if form.name.trim().is_empty() {
        return Err(AppError::bad_request("Give the garden a name."));
    }
    let model = match form.model.as_str() {
        "studio2" => DeviceModel::Studio2,
        "studio1" => DeviceModel::Studio1,
        "home4" => DeviceModel::Home4,
        "home3" => DeviceModel::Home3,
        "simulated" => DeviceModel::Simulated,
        _ => return Err(AppError::bad_request("Pick a model.")),
    };

    let now = state.now();
    // A simulated garden is backdated so it arrives mid-season with plants at
    // different stages and real work outstanding. Created "now" it would show a full
    // tank and ungerminated seeds — technically correct, and useless as a demo.
    let created_at = if model == DeviceModel::Simulated {
        garden_core::time::add_days(now, -SIMULATED_GARDEN_AGE_DAYS)
    } else {
        now
    };

    let garden = state
        .store
        .create_garden(
            form.name.trim(),
            model,
            form.timezone.as_deref().unwrap_or("UTC"),
            actor.id(),
            created_at,
        )
        .await?;

    state
        .store
        .log_event(
            garden.id,
            "garden.created",
            Some(&format!("{} created this garden", actor.user.label())),
            Some(actor.id()),
            now,
        )
        .await?;

    // A simulated garden arrives with plants already in it, written as ordinary rows
    // so they can be harvested, replaced or pulled like any others.
    demo::seed_plantings(&state.store, &garden, Some(actor.id())).await?;

    Ok(Redirect::to(&format!("/gardens/{}", garden.id)).into_response())
}

async fn detail(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
) -> Result<Markup, AppError> {
    let id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    let (garden, role) = authorize(&state, &actor, id, Permission::ViewGarden).await?;
    let now = state.now();

    // Refresh outstanding work from the rule engine before rendering. This now runs
    // for every garden, not just simulated ones: plantings alone are enough for the
    // calendar rules to have something useful to say.
    let snapshot = crate::state::build(&state.store, &garden, now).await?;
    let evaluation = garden_rules::default_engine().evaluate(&snapshot);
    state.store.sync_tasks(id, &evaluation.tasks, now).await?;
    demo::ensure_frame(&state.store, &garden, &snapshot, now).await?;

    let has_telemetry = crate::state::has_telemetry(&snapshot);
    let latest_frame = state.store.latest_frame(id).await?;

    let tasks = state.store.tasks_for(id).await?;
    let members = state.store.members_of(id).await?;
    let events = state.store.recent_events(id, 8).await?;
    let components = state.store.components_for(id).await?;
    let can_act = role.grants(Permission::CompleteTask);

    let actionable: Vec<&TaskRecord> = tasks.iter().filter(|t| t.is_actionable(now)).collect();

    Ok(ui::page(
        &garden.name,
        Some(&actor),
        html! {
            div.row {
                div {
                    h1 { (garden.name) }
                    p.muted.small style="margin:0" {
                        (garden.model.label())
                        " · " (garden.timezone)
                        " · you are " (role.label())
                    }
                }
                div.spacer {}
                a.button href=(format!("/gardens/{id}/slots")) {
                    "Slots (" (snapshot.occupied_slots()) ")"
                }
                a.button href=(format!("/gardens/{id}/frames")) { "Camera" }
                a.button href=(format!("/gardens/{id}/members")) {
                    "Sharing (" (members.len()) ")"
                }
                @if actor.can(garden.id, Permission::ControlHardware) {
                    a.button href=(format!("/gardens/{id}/schedule")) { "Schedule" }
                }
                @if actor.can(garden.id, Permission::ConfigureGarden) {
                    a.button href=(format!("/gardens/{id}/storage")) { "Storage" }
                }
            }

            @if let Some(frame) = &latest_frame {
                a.card href=(format!("/gardens/{id}/frames"))
                  style="display:block; text-decoration:none; color:inherit" {
                    img src=(frame.image_path()) alt="Latest camera frame"
                        style="width:100%; border-radius:8px; display:block";
                    p.small.muted style="margin:0.5rem 0 0" {
                        "Camera · "
                        (ui::relative(now.as_second() - frame.captured_at.as_second()))
                        @if !frame.comparable {
                            " · ambient light, colour not comparable"
                        }
                    }
                }
            }

            @if has_telemetry {
                (sensors(&snapshot))
            } @else {
                div.card {
                    h3 { "No sensors reporting" }
                    p.muted.small style="margin:0" {
                        "Nothing is measuring this garden yet — the edge agent registers \
                         itself the first time it runs. Everything below is worked out \
                         from what you have planted and when, which is enough for \
                         thinning, harvest timing, root checks and replanting."
                    }
                }
            }

            h2 { "What needs doing" }
            @if actionable.is_empty() {
                div.card { p.muted style="margin:0" { "Nothing outstanding." } }
            }
            @for task in &actionable {
                (task_card(task, id, can_act, now))
            }

            div.row style="margin-top:2rem" {
                h2 style="margin:0" { "Slots" }
                div.spacer {}
                a.small href=(format!("/gardens/{id}/slots")) { "manage" }
            }
            (slots(&snapshot))

            @if !components.is_empty() {
                h2 { "Hardware" }
                div.grid {
                    @for component in &components {
                        div.card {
                            div.row {
                                strong { (component.name) }
                                div.spacer {}
                                (ui::health_pill(component.health(now)))
                            }
                            p.small.muted style="margin:0.3rem 0 0" {
                                (component.kind)
                                @if let Some(seconds) = component.seconds_since_seen(now) {
                                    " · seen " (ui::relative(seconds))
                                }
                            }
                        }
                    }
                }
            }

            h2 { "Recent activity" }
            div.card {
                @if events.is_empty() {
                    p.muted.small style="margin:0" { "Nothing logged yet." }
                }
                @for event in &events {
                    p.small style="margin:0 0 0.4rem" {
                        span.muted { (ui::relative(now.as_second() - event.occurred_at.as_second())) }
                        " — "
                        (event.detail.clone().unwrap_or_else(|| event.kind.clone()))
                        @if let Some(name) = &event.actor_name {
                            span.muted { " (" (name) ")" }
                        }
                    }
                }
            }

            @if role.grants(Permission::ConfigureGarden) {
                h2 { "Settings" }
                form.card method="post" action=(format!("/gardens/{id}/settings")) {
                    label for="name" { "Name" }
                    input #name type="text" name="name" value=(garden.name) required;
                    label for="timezone" { "Timezone" }
                    input #timezone type="text" name="timezone" value=(garden.timezone);
                    p style="margin-top:0.75rem" { button type="submit" { "Save" } }
                }
            }

            @if role.grants(Permission::DeleteGarden) {
                form.card method="post" action=(format!("/gardens/{id}/delete"))
                     onsubmit="return confirm('Delete this garden and all its history? This cannot be undone.')" {
                    h3 { "Delete this garden" }
                    p.small.muted { "Removes its history and everyone's access to it." }
                    button.danger type="submit" { "Delete garden" }
                }
            }
        },
    ))
}

fn sensors(state: &GardenState) -> Markup {
    let fill = state.fill_fraction() * 100.0;
    let days = state
        .tank
        .days_until(state.tank_geometry.capacity_l * 0.15)
        .filter(|d| *d > 0.0);

    html! {
        div.grid {
            div.card {
                div.stat-label { "Tank" }
                div.stat { (format!("{fill:.0}%")) }
                p.small.muted style="margin:0" {
                    (format!("{:.1} L", state.tank.volume_l))
                    @if let Some(days) = days {
                        " · " (format!("{days:.1} days left"))
                    }
                }
            }
            @if let Some(temp) = state.sensors.air_temp_c {
                div.card {
                    div.stat-label { "Air" }
                    div.stat { (format!("{temp:.1}°C")) }
                    @if let Some(humidity) = state.sensors.humidity_pct {
                        p.small.muted style="margin:0" { (format!("{humidity:.0}% RH")) }
                    }
                }
            }
            @if let Some(temp) = state.sensors.water_temp_c {
                div.card {
                    div.stat-label { "Reservoir" }
                    div.stat { (format!("{temp:.1}°C")) }
                    p.small.muted style="margin:0" { "root zone" }
                }
            }
            div.card {
                div.stat-label { "Pump" }
                div.stat { (format!("{:.0}%", (state.pump.restriction_ratio() - 1.0) * 100.0)) }
                p.small.muted style="margin:0" { "above clean baseline" }
            }
            @if let Some(ec) = state.sensors.ec_ms_cm {
                div.card {
                    div.stat-label { "EC" }
                    div.stat { (format!("{ec:.2}")) }
                    p.small.muted style="margin:0" { "mS/cm" }
                }
            }
            div.card {
                div.stat-label { "Using" }
                div.stat { (format!("{:.2}", state.tank.consumption_lpd)) }
                p.small.muted style="margin:0" { "litres/day" }
            }
        }
    }
}

/// The tower as it physically stands: one screen column per real column.
fn slots(state: &GardenState) -> Markup {
    let geometry = state.geometry;
    html! {
        div.tower style=(format!(
            "grid-template-columns: repeat({}, minmax(0, 1fr))",
            geometry.columns.max(1)
        )) {
            @for column in 0..geometry.columns {
                div.tower-column {
                    div.tower-head { "column " (column + 1) }
                    @for slot in geometry.column(column) {
                        @let zone = geometry.light_zone(slot);
                        div.slot-row {
                            div.zone-strip class=(format!("zone-{}", zone.slug()))
                                title=(zone.label()) {}
                            @match state.planting_in(slot) {
                                Some(planting) => {
                                    @let variety = state.variety_of(planting);
                                    div.slot {
                                        strong.small { (slot.to_string()) }
                                        br;
                                        @match variety {
                                            Some(v) => {
                                                span { (v.name) }
                                                br;
                                                span.muted { (planting.stage(v, state.now).label()) }
                                            }
                                            None => span.muted { "unknown variety" }
                                        }
                                    }
                                }
                                None => div.slot.empty {
                                    strong.small { (slot.to_string()) }
                                    br;
                                    span { "empty" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn task_card(task: &TaskRecord, garden: GardenId, can_act: bool, now: jiff::Timestamp) -> Markup {
    html! {
        div.card {
            div.row {
                (ui::severity_pill(task.severity))
                strong { (task.kind) }
                span.muted.small { (task.target) }
                @if task.is_overdue(now) {
                    span.pill.sev-urgent { "overdue" }
                }
                div.spacer {}
                @if can_act {
                    form method="post"
                         action=(format!("/gardens/{garden}/tasks/{}/complete", task.key)) {
                        button.primary type="submit" { "Done" }
                    }
                    form method="post"
                         action=(format!("/gardens/{garden}/tasks/{}/snooze", task.key)) {
                        button type="submit" { "Snooze" }
                    }
                    form method="post"
                         action=(format!("/gardens/{garden}/tasks/{}/dismiss", task.key)) {
                        button.link type="submit" { "N/A" }
                    }
                }
            }
            p.small style="margin:0.5rem 0 0" { (task.rationale) }
            p.small.muted style="margin:0.25rem 0 0" {
                @if let Some(detail) = &task.detail { (detail) " · " }
                "from " code { (task.source_rule) }
            }
        }
    }
}

async fn update(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
    Form(form): Form<NewGardenForm>,
) -> Result<Response, AppError> {
    let id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    authorize(&state, &actor, id, Permission::ConfigureGarden).await?;

    if form.name.trim().is_empty() {
        return Err(AppError::bad_request("A garden needs a name."));
    }
    state
        .store
        .rename_garden(id, &form.name, form.timezone.as_deref().unwrap_or("UTC"))
        .await?;
    Ok(Redirect::to(&format!("/gardens/{id}")).into_response())
}

async fn delete(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    authorize(&state, &actor, id, Permission::DeleteGarden).await?;
    state.store.delete_garden(id).await?;
    Ok(Redirect::to("/").into_response())
}
