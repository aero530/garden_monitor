//! The background loop that actually gets a task onto your phone.
//!
//! Runs inside the brain on a timer. For every garden: rebuild state, evaluate the
//! rules, reconcile tasks, then for every member decide whether anything warrants
//! interrupting them and send it.
//!
//! One property matters more than the rest: **each member is decided independently**.
//! Two people share a garden, sleep at different hours and have different phones; a
//! notification held for one must still reach the other.

use crate::app::AppState;
use gardyn_auth::{Permission, TaskAction};
use gardyn_core::{Garden, Severity, TaskKey, TaskKind};
use gardyn_notify::{NotificationAction, compose, compose_brief, decide, reach_for};
use gardyn_store::notifications::NotificationPrefs;
use gardyn_store::tasks::TaskRecord;
use jiff::Timestamp;
use std::time::Duration;

/// How often the loop runs.
///
/// Five minutes is well inside the resolution of anything the rules reason about —
/// the fastest is water, measured in hours — while keeping a Critical escalation
/// prompt enough to matter.
pub const TICK: Duration = Duration::from_secs(300);

/// Start the dispatcher.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        // A short delay so startup logs are not interleaved with the first sweep.
        tokio::time::sleep(Duration::from_secs(10)).await;
        loop {
            if let Err(error) = sweep(&state).await {
                // Never let one bad garden kill the loop for every other one.
                tracing::error!(%error, "notification sweep failed");
            }
            tokio::time::sleep(TICK).await;
        }
    });
}

async fn sweep(state: &AppState) -> Result<(), gardyn_store::StoreError> {
    let now = state.now();
    let gardens = state.store.all_gardens().await?;

    for garden in gardens {
        if let Err(error) = sweep_garden(state, &garden, now).await {
            tracing::error!(garden = %garden.id, %error, "skipping garden");
        }
    }
    Ok(())
}

async fn sweep_garden(
    state: &AppState,
    garden: &Garden,
    now: Timestamp,
) -> Result<(), gardyn_store::StoreError> {
    // Refresh outstanding work first. The dispatcher must not notify from a stale
    // task list — that is how someone gets pinged about a tank they filled an hour ago.
    let snapshot = crate::state::build(&state.store, garden, now).await?;
    let evaluation = gardyn_rules::default_engine().evaluate(&snapshot);
    state.store.sync_tasks(garden.id, &evaluation.tasks, now).await?;
    state.store.prune_notifications(garden.id).await?;

    let Some(notifier) = state.notifier.as_ref() else {
        return Ok(()); // Nothing configured; the web UI still shows everything.
    };

    let tasks: Vec<TaskRecord> = state
        .store
        .tasks_for(garden.id)
        .await?
        .into_iter()
        .filter(|t| t.is_actionable(now))
        .collect();
    if tasks.is_empty() {
        return Ok(());
    }

    for member in state.store.members_of(garden.id).await? {
        // Someone who cannot act on tasks should not be pinged about them. A viewer
        // asked to see the garden, not to be woken by it.
        if !member.role.grants(Permission::CompleteTask) {
            continue;
        }
        let prefs = state.store.notification_prefs(member.user.id).await?;
        if !prefs.wants_anything() {
            continue;
        }

        // Most severe first, so a burst that hits the cap drops the least important
        // rather than whatever happened to sort first.
        let mut ordered: Vec<&TaskRecord> = tasks.iter().collect();
        ordered.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.due_at.cmp(&b.due_at)));

        let mut sent = 0;
        for task in ordered {
            if sent >= MAX_BURST {
                tracing::info!(
                    garden = %garden.id,
                    held = tasks.len() - sent,
                    "burst cap reached; the rest wait for the next sweep or the brief"
                );
                break;
            }
            if deliver_one(state, notifier, garden, &prefs, &member.user, task, now).await? {
                sent += 1;
            }
        }
        deliver_brief(state, notifier, garden, &prefs, &member.user, &tasks, now).await?;
    }
    Ok(())
}

/// Most interrupting notifications one person may receive about one garden in a
/// single sweep.
///
/// Without this, the first sweep of a neglected garden fires everything at once — a
/// real run produced seventeen. Nobody reads seventeen notifications; they mute the
/// app, and then the one that mattered is lost too. Anything over the cap waits for
/// the next sweep or lands in the morning brief.
const MAX_BURST: usize = 3;

/// The hour, local to the recipient, at which the daily brief goes out.
const BRIEF_HOUR: u8 = 8;

/// A pseudo task key, so the brief reuses the same once-per-thing bookkeeping as
/// everything else rather than needing its own table.
const BRIEF_KEY: &str = "__daily_brief";

/// Send the morning brief.
///
/// This is where advisories live. Without it the Advisory tier is meaningless — the
/// only choices would be interrupting someone about a root check or never mentioning
/// it, and both are wrong.
async fn deliver_brief(
    state: &AppState,
    notifier: &gardyn_notify::Notifier,
    garden: &Garden,
    prefs: &NotificationPrefs,
    user: &gardyn_auth::User,
    tasks: &[TaskRecord],
    now: Timestamp,
) -> Result<(), gardyn_store::StoreError> {
    if prefs.local_hour(now) != BRIEF_HOUR {
        return Ok(());
    }
    let key = TaskKey(BRIEF_KEY.to_string());

    // At most one a day, whatever the sweep interval.
    if let Some(last) = state.store.last_notified(garden.id, user.id, &key).await?
        && gardyn_core::time::days_between(last.at, now) < 0.9
    {
        return Ok(());
    }

    let lines = brief_lines(tasks, now);
    if lines.is_empty() {
        return Ok(());
    }

    let note = compose_brief(
        &garden.name,
        &lines,
        Some(format!("{}/gardens/{}", state.config.base_url, garden.id)),
    );
    // Push only. A daily digest by email is how a mailbox learns to filter you.
    let reach = gardyn_notify::Reach {
        push: true,
        email: false,
        priority: note.priority,
        interrupts: false,
    };

    let delivered = notifier
        .deliver(&note, reach, prefs.ntfy_topic.as_deref(), None)
        .await;
    if delivered.any() {
        state
            .store
            .record_notification(garden.id, user.id, &key, Severity::Advisory, "push", now)
            .await?;
        tracing::info!(garden = %garden.id, items = lines.len(), "sent the daily brief");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn deliver_one(
    state: &AppState,
    notifier: &gardyn_notify::Notifier,
    garden: &Garden,
    prefs: &NotificationPrefs,
    user: &gardyn_auth::User,
    task: &TaskRecord,
    now: Timestamp,
) -> Result<bool, gardyn_store::StoreError> {
    let last = state.store.last_notified(garden.id, user.id, &task.key).await?;
    let decision = decide(
        task.severity,
        last,
        prefs.quiet,
        prefs.local_hour(now),
        now,
    );
    if !decision.should_send() {
        return Ok(false);
    }

    let kind = parse_kind(&task.kind).unwrap_or(TaskKind::Inspect);
    let reach = reach_for(task.severity);

    // One-tap buttons. Each is a single-use grant scoped to this person and this task,
    // so a link that leaks from a lock screen cannot be replayed or aimed elsewhere.
    let actions = build_actions(state, user.id, garden.id, &task.key, now).await?;

    let note = compose(
        kind,
        &task.target,
        &garden.name,
        &task.rationale,
        task.detail.as_deref(),
        task.severity,
        reach.priority,
        Some(format!("{}/gardens/{}", state.config.base_url, garden.id)),
        actions,
    );

    let delivered = notifier
        .deliver(
            &note,
            reach,
            prefs.ntfy_topic.as_deref(),
            prefs.email_enabled.then(|| user.email.as_str()),
        )
        .await;

    if !delivered.any() {
        // Not recorded, so the next sweep retries rather than treating a failed
        // delivery as done and going quiet.
        tracing::warn!(task = %task.key, user = %user.id, "no channel accepted the notification");
        return Ok(false);
    }

    let channels = match (delivered.push, delivered.email) {
        (true, true) => "push,email",
        (true, false) => "push",
        _ => "email",
    };
    state
        .store
        .record_notification(garden.id, user.id, &task.key, task.severity, channels, now)
        .await?;

    tracing::info!(
        task = %task.key,
        severity = %task.severity,
        %channels,
        reason = ?decision,
        "notified"
    );
    Ok(true)
}

async fn build_actions(
    state: &AppState,
    user: gardyn_auth::UserId,
    garden: gardyn_core::GardenId,
    key: &TaskKey,
    now: Timestamp,
) -> Result<Vec<NotificationAction>, gardyn_store::StoreError> {
    let mut actions = Vec::new();
    for (action, label) in [
        (TaskAction::Complete, "Done"),
        (TaskAction::Snooze, "Snooze"),
        (TaskAction::Dismiss, "N/A"),
    ] {
        let token = state
            .store
            .issue_action_grant(user, garden, key.clone(), action, now)
            .await?;
        actions.push(NotificationAction {
            label: label.to_string(),
            url: format!("{}/a/{}", state.config.base_url, token.expose()),
        });
    }
    Ok(actions)
}

/// `TaskRecord` stores the kind as its display label; map it back for the icon.
fn parse_kind(label: &str) -> Option<TaskKind> {
    [
        TaskKind::AddWater,
        TaskKind::AddPlantFood,
        TaskKind::AddConditioner,
        TaskKind::PruneRoots,
        TaskKind::PrunePlant,
        TaskKind::Harvest,
        TaskKind::Thin,
        TaskKind::Pollinate,
        TaskKind::TankRefresh,
        TaskKind::DeepClean,
        TaskKind::Replant,
        TaskKind::Inspect,
    ]
    .into_iter()
    .find(|k| k.label() == label)
}

/// Everything too quiet to interrupt about, for the daily brief.
pub fn brief_lines(tasks: &[TaskRecord], now: Timestamp) -> Vec<String> {
    tasks
        .iter()
        .filter(|t| t.is_actionable(now))
        .filter(|t| t.severity < Severity::Important)
        .map(|t| match &t.detail {
            Some(detail) => format!("{} ({}) — {}", t.kind, detail, t.target),
            None => format!("{} — {}", t.kind, t.target),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_task_kind_round_trips_through_its_label() {
        // The dispatcher recovers the kind from stored display text to pick an icon;
        // a miss would silently degrade every notification to the generic one.
        for kind in [
            TaskKind::AddWater,
            TaskKind::AddPlantFood,
            TaskKind::AddConditioner,
            TaskKind::PruneRoots,
            TaskKind::PrunePlant,
            TaskKind::Harvest,
            TaskKind::Thin,
            TaskKind::Pollinate,
            TaskKind::TankRefresh,
            TaskKind::DeepClean,
            TaskKind::Replant,
            TaskKind::Inspect,
        ] {
            assert_eq!(parse_kind(kind.label()), Some(kind), "{kind} did not survive");
        }
    }

    #[test]
    fn an_unknown_label_does_not_panic() {
        assert_eq!(parse_kind("something we renamed"), None);
    }

    #[test]
    fn the_tick_is_fast_enough_to_matter_for_water() {
        // The fastest thing the rules reason about is measured in hours.
        assert!(TICK <= Duration::from_secs(900));
    }
}
