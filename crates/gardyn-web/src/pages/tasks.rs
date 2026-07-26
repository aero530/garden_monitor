//! Acting on tasks, from the dashboard and from a notification.

use crate::app::{AppState, Auth};
use crate::error::AppError;
use crate::pages::gardens::authorize;
use crate::ui;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Router, routing::get, routing::post};
use gardyn_auth::{Actor, Permission, TaskAction};
use gardyn_core::{GardenId, TaskKey};
use gardyn_core::time::add_days;
use maud::html;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/gardens/{id}/tasks/{key}/complete", post(complete))
        .route("/gardens/{id}/tasks/{key}/snooze", post(snooze))
        .route("/gardens/{id}/tasks/{key}/dismiss", post(dismiss))
        .route("/a/{token}", get(one_tap))
}

/// How long "snooze" defers a task.
const SNOOZE_DAYS: f64 = 1.0;

async fn apply(
    state: &AppState,
    actor: &Actor,
    garden: GardenId,
    key: &TaskKey,
    action: TaskAction,
) -> Result<(), AppError> {
    let now = state.now();
    actor.require(garden, action.required_permission())?;

    let Some(task) = state.store.find_task(garden, key).await? else {
        return Err(AppError::NotFound);
    };

    match action {
        TaskAction::Complete => {
            state
                .store
                .complete_task(garden, key, actor.id(), now)
                .await?;

            // Close the loop with the plant itself.
            //
            // The rule engine is stateless and re-derives from stored state, so
            // ticking "prune roots" has to move `last_root_check` or the identical
            // task reappears on the next evaluation. Marking the task done without
            // this would make completion look like it worked and then silently undo
            // itself.
            record_against_planting(state, garden, &task, now).await?;
            record_against_tank(state, garden, &task, actor, now).await?;

            state
                .store
                .log_event(
                    garden,
                    "task.completed",
                    Some(&format!("{} — {}", task.kind, task.target)),
                    Some(actor.id()),
                    now,
                )
                .await?;
        }
        TaskAction::Snooze => {
            state
                .store
                .snooze_task(garden, key, add_days(now, SNOOZE_DAYS))
                .await?;
        }
        TaskAction::Dismiss => {
            state.store.dismiss_task(garden, key).await?;
            state
                .store
                .log_event(
                    garden,
                    "task.dismissed",
                    Some(&format!("{} — not applicable", task.kind)),
                    Some(actor.id()),
                    now,
                )
                .await?;
        }
    }
    Ok(())
}

/// Stamp a completed plant-level task onto its planting.
///
/// The task's own target is the source of truth for *which* plant, and it is parsed
/// from the stored record rather than from the URL, so a task key cannot be used to
/// aim an update at some other planting.
async fn record_against_planting(
    state: &AppState,
    garden: GardenId,
    task: &gardyn_store::tasks::TaskRecord,
    now: jiff::Timestamp,
) -> Result<(), AppError> {
    let Some(planting) = planting_target(&task.target) else {
        return Ok(());
    };
    let Some(event) = kind_to_event(&task.kind) else {
        return Ok(());
    };
    state
        .store
        .record_planting_event(garden, planting, event, now)
        .await?;
    Ok(())
}

/// Close the same loop for the tank.
///
/// Feed, condition, refresh and deep clean are garden-level: they have no planting to
/// write back to, and until this existed they wrote back to nothing at all. The rule
/// engine re-derives "overdue for a refresh" from `last_refresh` on every tick, so
/// completing one of these without recording it marked the task done and then produced
/// the identical task again five minutes later, forever.
async fn record_against_tank(
    state: &AppState,
    garden: GardenId,
    task: &gardyn_store::tasks::TaskRecord,
    actor: &Actor,
    now: jiff::Timestamp,
) -> Result<(), AppError> {
    let Some(kind) = parse_task_kind(&task.kind) else {
        return Ok(());
    };
    let geometry = gardyn_core::TankGeometry::STUDIO_2;
    let Some(event) = gardyn_core::TankEvent::for_task(kind, &geometry) else {
        return Ok(());
    };
    state
        .store
        .record_tank_event(garden, event, Some(actor.id()), now)
        .await?;
    Ok(())
}

/// Recover a `TaskKind` from the label stored on the row.
fn parse_task_kind(label: &str) -> Option<gardyn_core::TaskKind> {
    use gardyn_core::TaskKind::*;
    [
        AddWater,
        AddPlantFood,
        AddConditioner,
        PruneRoots,
        PrunePlant,
        Harvest,
        Thin,
        Pollinate,
        TankRefresh,
        DeepClean,
        Replant,
        Inspect,
    ]
    .into_iter()
    .find(|k| k.label() == label)
}

/// Recover a planting id from the rendered target text ("planting 7").
///
/// `TaskRecord` stores the target as display text rather than a typed enum, so this
/// parses it back. Anything unrecognised means the task was not about a plant.
fn planting_target(target: &str) -> Option<gardyn_core::PlantingId> {
    target
        .strip_prefix("planting ")?
        .trim()
        .parse()
        .ok()
        .map(gardyn_core::PlantingId)
}

/// Map the stored task kind label back to a planting event.
fn kind_to_event(kind: &str) -> Option<gardyn_store::plantings::PlantingEvent> {
    use gardyn_core::TaskKind::*;
    let parsed = [PruneRoots, PrunePlant, Harvest, Thin]
        .into_iter()
        .find(|k| k.label() == kind)?;
    gardyn_store::plantings::event_for_task(parsed)
}

async fn act(
    state: AppState,
    actor: Actor,
    id: String,
    key: String,
    action: TaskAction,
) -> Result<Response, AppError> {
    let garden: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    authorize(&state, &actor, garden, Permission::CompleteTask).await?;
    apply(&state, &actor, garden, &TaskKey(key), action).await?;
    Ok(Redirect::to(&format!("/gardens/{garden}")).into_response())
}

async fn complete(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path((id, key)): Path<(String, String)>,
) -> Result<Response, AppError> {
    act(state, actor, id, key, TaskAction::Complete).await
}

async fn snooze(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path((id, key)): Path<(String, String)>,
) -> Result<Response, AppError> {
    act(state, actor, id, key, TaskAction::Snooze).await
}

async fn dismiss(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path((id, key)): Path<(String, String)>,
) -> Result<Response, AppError> {
    act(state, actor, id, key, TaskAction::Dismiss).await
}

/// A one-tap link from a push notification.
///
/// Two independent checks have to pass. The grant proves the link was issued to a
/// specific person for a specific task and has not been used; the actor's membership
/// proves they are *still* allowed to act. Redeeming a valid link for someone who was
/// removed from the garden last week must fail.
///
/// Redeeming does not sign anyone in. It ticks one task and shows a confirmation.
async fn one_tap(
    State(state): State<AppState>,
    Path(raw): Path<String>,
) -> Result<Response, AppError> {
    let now = state.now();

    let Some(token) = gardyn_auth::SecretToken::from_client(&raw) else {
        return Ok(ui::error_page("Invalid link", "That link is not valid.").into_response());
    };

    let grant = match state.store.redeem_action_grant(&token, now).await? {
        Ok(grant) => grant,
        Err(reason) => {
            return Ok(ui::error_page(
                "That link no longer works",
                &format!("{reason}."),
            )
            .into_response());
        }
    };

    // Load the recipient fresh, so removed access takes effect immediately.
    let Some(user) = state.store.find_user(grant.user).await? else {
        return Ok(ui::error_page("That link no longer works", "The account is gone.")
            .into_response());
    };
    let memberships = state.store.memberships_of_user(grant.user).await?;
    let actor = Actor::new(user, memberships);

    if actor
        .require(grant.garden, grant.action.required_permission())
        .is_err()
    {
        return Ok(ui::error_page(
            "That link no longer works",
            "You no longer have permission to act on this garden.",
        )
        .into_response());
    }

    apply(&state, &actor, grant.garden, &grant.task, grant.action).await?;
    let garden = state.store.find_garden(grant.garden).await?;
    let name = garden.map(|g| g.name).unwrap_or_else(|| "your garden".into());

    Ok(ui::plain_page(
        "Done",
        html! {
            h1 { "✅ " (grant.action.label()) }
            p.muted { "Recorded for " strong { (name) } "." }
            @if grant.action == TaskAction::Complete {
                p.small.muted {
                    "If the sensors do not confirm this shortly, the task will come back."
                }
            }
            // The link did not create a session, so this is an invitation to sign in,
            // not a link into an already-authenticated app.
            p { a.button href=(format!("/gardens/{}", grant.garden)) { "Open the garden" } }
        },
    )
    .into_response())
}
