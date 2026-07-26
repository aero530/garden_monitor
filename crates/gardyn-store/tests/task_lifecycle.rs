//! Task lifecycle against a real database.
//!
//! The rule engine is stateless and re-emits everything every tick. All the
//! interesting behaviour — dedupe, completion, snoozing, and the auto-verification
//! reopen — lives in the reconciliation here, so this is where it gets tested.

use gardyn_auth::{EmailAddress, TaskAction};
use gardyn_core::{
    DeviceModel, DueWindow, GardenId, RuleId, Severity, Target, Task, TaskDetail, TaskKey, TaskKind,
    time::add_days,
};
use gardyn_store::Store;
use gardyn_store::tasks::{TaskState, VERIFY_WINDOW_MINUTES};

fn t0() -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_700_000_000).unwrap()
}

fn minutes(n: f64) -> f64 {
    n / (24.0 * 60.0)
}

fn water_task(now: jiff::Timestamp, severity: Severity) -> Task {
    Task::new(
        TaskKind::AddWater,
        Target::Garden,
        severity,
        DueWindow::within_days(now, 2.0),
        "tank at 22%, using 0.5 L/day",
        RuleId::from_static("water-level"),
    )
    .with_detail(TaskDetail::Water { litres: 4.0 })
}

fn harvest_task(now: jiff::Timestamp) -> Task {
    Task::new(
        TaskKind::Harvest,
        Target::Slot(gardyn_core::SlotId(3)),
        Severity::Advisory,
        DueWindow::within_days(now, 4.0),
        "kale is due",
        RuleId::from_static("harvest-by-calendar"),
    )
}

async fn garden() -> (Store, GardenId, gardyn_auth::UserId) {
    let store = Store::in_memory().await.unwrap();
    let user = store
        .create_user(
            EmailAddress::parse("phil@example.com").unwrap(),
            "Phil",
            "a long enough password",
            t0(),
        )
        .await
        .unwrap();
    let garden = store
        .create_garden("Kitchen", DeviceModel::Studio2, "UTC", user.id, t0())
        .await
        .unwrap();
    (store, garden.id, user.id)
}

#[tokio::test]
async fn a_new_task_is_opened() {
    let (store, garden, _) = garden().await;
    let outcome = store
        .sync_tasks(garden, &[water_task(t0(), Severity::Important)], t0())
        .await
        .unwrap();

    assert_eq!(outcome.opened, 1);
    let tasks = store.tasks_for(garden).await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].state, TaskState::Open);
    assert!(tasks[0].is_actionable(t0()));
    assert_eq!(tasks[0].detail.as_deref(), Some("4.0 L"));
}

#[tokio::test]
async fn re_emitting_the_same_task_does_not_duplicate_it() {
    // The rules emit continuously; the operator must see one task, not one per tick.
    let (store, garden, _) = garden().await;
    for tick in 0..10 {
        let now = add_days(t0(), f64::from(tick) * 0.01);
        store
            .sync_tasks(garden, &[water_task(now, Severity::Important)], now)
            .await
            .unwrap();
    }
    assert_eq!(store.tasks_for(garden).await.unwrap().len(), 1);
}

#[tokio::test]
async fn wording_and_urgency_refresh_without_resetting_the_lifecycle() {
    let (store, garden, _) = garden().await;
    store
        .sync_tasks(garden, &[water_task(t0(), Severity::Advisory)], t0())
        .await
        .unwrap();
    let first_seen = store.tasks_for(garden).await.unwrap()[0].first_seen_at;

    let later = add_days(t0(), 0.5);
    store
        .sync_tasks(garden, &[water_task(later, Severity::Critical)], later)
        .await
        .unwrap();

    let task = &store.tasks_for(garden).await.unwrap()[0];
    assert_eq!(task.severity, Severity::Critical, "urgency should escalate");
    assert_eq!(task.first_seen_at, first_seen, "but it is still the same task");
}

#[tokio::test]
async fn a_task_the_rules_stop_emitting_resolves_itself() {
    // The condition went away. Nothing to act on, so nothing to nag about.
    let (store, garden, _) = garden().await;
    store
        .sync_tasks(garden, &[water_task(t0(), Severity::Important)], t0())
        .await
        .unwrap();

    let outcome = store.sync_tasks(garden, &[], add_days(t0(), 0.1)).await.unwrap();
    assert_eq!(outcome.resolved, 1);
    assert!(store.tasks_for(garden).await.unwrap().is_empty());
}

#[tokio::test]
async fn completing_a_task_takes_it_off_the_list() {
    let (store, garden, user) = garden().await;
    let key = TaskKey::new(TaskKind::AddWater, Target::Garden);
    store
        .sync_tasks(garden, &[water_task(t0(), Severity::Important)], t0())
        .await
        .unwrap();

    store.complete_task(garden, &key, user, t0()).await.unwrap();

    let task = store.find_task(garden, &key).await.unwrap().unwrap();
    assert_eq!(task.state, TaskState::Done);
    assert_eq!(task.completed_by, Some(user));
    assert!(!task.is_actionable(t0()));
}

#[tokio::test]
async fn a_completed_task_stays_quiet_inside_the_verification_window() {
    // The tank takes a moment to register a top-up; don't nag in the meantime.
    let (store, garden, user) = garden().await;
    let key = TaskKey::new(TaskKind::AddWater, Target::Garden);
    store
        .sync_tasks(garden, &[water_task(t0(), Severity::Important)], t0())
        .await
        .unwrap();
    store.complete_task(garden, &key, user, t0()).await.unwrap();

    let soon = add_days(t0(), minutes(VERIFY_WINDOW_MINUTES / 2.0));
    let outcome = store
        .sync_tasks(garden, &[water_task(soon, Severity::Important)], soon)
        .await
        .unwrap();

    assert_eq!(outcome.reopened, 0);
    let task = store.find_task(garden, &key).await.unwrap().unwrap();
    assert_eq!(task.state, TaskState::Done);
    assert!(!task.is_actionable(soon));
}

#[tokio::test]
async fn an_unverified_completion_reopens() {
    // The closed loop. Claiming to have watered does not make the tank full: if the
    // rules still want water after the window, the task comes back on its own.
    let (store, garden, user) = garden().await;
    let key = TaskKey::new(TaskKind::AddWater, Target::Garden);
    store
        .sync_tasks(garden, &[water_task(t0(), Severity::Important)], t0())
        .await
        .unwrap();
    store.complete_task(garden, &key, user, t0()).await.unwrap();

    let later = add_days(t0(), minutes(VERIFY_WINDOW_MINUTES + 5.0));
    let outcome = store
        .sync_tasks(garden, &[water_task(later, Severity::Urgent)], later)
        .await
        .unwrap();

    assert_eq!(outcome.reopened, 1);
    let task = store.find_task(garden, &key).await.unwrap().unwrap();
    assert_eq!(task.state, TaskState::Open);
    assert_eq!(task.completed_at, None, "the false completion is cleared");
    assert_eq!(task.completed_by, None);
    assert!(task.is_actionable(later));
}

#[tokio::test]
async fn a_verified_completion_does_not_come_back() {
    // The counterpart: the operator really did water, so the rules stop asking.
    let (store, garden, user) = garden().await;
    let key = TaskKey::new(TaskKind::AddWater, Target::Garden);
    store
        .sync_tasks(garden, &[water_task(t0(), Severity::Important)], t0())
        .await
        .unwrap();
    store.complete_task(garden, &key, user, t0()).await.unwrap();

    let later = add_days(t0(), minutes(VERIFY_WINDOW_MINUTES + 5.0));
    store.sync_tasks(garden, &[], later).await.unwrap();
    assert!(store.find_task(garden, &key).await.unwrap().is_none());
}

#[tokio::test]
async fn snoozing_hides_a_task_until_its_time() {
    let (store, garden, _) = garden().await;
    let key = TaskKey::new(TaskKind::AddWater, Target::Garden);
    store
        .sync_tasks(garden, &[water_task(t0(), Severity::Advisory)], t0())
        .await
        .unwrap();

    let until = add_days(t0(), 1.0);
    store.snooze_task(garden, &key, until).await.unwrap();

    let task = store.find_task(garden, &key).await.unwrap().unwrap();
    assert!(!task.is_actionable(t0()));
    assert!(!task.is_actionable(add_days(t0(), 0.5)));
    assert!(task.is_actionable(add_days(t0(), 1.1)), "should come back");
}

#[tokio::test]
async fn dismissing_a_task_keeps_it_dismissed() {
    let (store, garden, _) = garden().await;
    let key = TaskKey::new(TaskKind::AddWater, Target::Garden);
    store
        .sync_tasks(garden, &[water_task(t0(), Severity::Advisory)], t0())
        .await
        .unwrap();
    store.dismiss_task(garden, &key).await.unwrap();

    // Still emitted by the rules, but the operator said it does not apply.
    let later = add_days(t0(), 1.0);
    store
        .sync_tasks(garden, &[water_task(later, Severity::Advisory)], later)
        .await
        .unwrap();

    let task = store.find_task(garden, &key).await.unwrap().unwrap();
    assert_eq!(task.state, TaskState::Dismissed);
    assert!(!task.is_actionable(later));
}

#[tokio::test]
async fn tasks_are_ranked_by_severity() {
    let (store, garden, _) = garden().await;
    store
        .sync_tasks(
            garden,
            &[harvest_task(t0()), water_task(t0(), Severity::Critical)],
            t0(),
        )
        .await
        .unwrap();

    let tasks = store.tasks_for(garden).await.unwrap();
    assert_eq!(tasks[0].severity, Severity::Critical);
    assert_eq!(tasks[1].kind, "harvest");
}

#[tokio::test]
async fn one_gardens_tasks_never_appear_in_another() {
    let (store, kitchen, user) = garden().await;
    let office = store
        .create_garden("Office", DeviceModel::Studio2, "UTC", user, t0())
        .await
        .unwrap();

    store
        .sync_tasks(kitchen, &[water_task(t0(), Severity::Critical)], t0())
        .await
        .unwrap();

    assert_eq!(store.tasks_for(kitchen).await.unwrap().len(), 1);
    assert_eq!(store.tasks_for(office.id).await.unwrap().len(), 0);

    // And syncing one garden must not sweep away the other's work.
    store.sync_tasks(office.id, &[], t0()).await.unwrap();
    assert_eq!(store.tasks_for(kitchen).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_one_tap_link_works_once_and_only_for_its_own_task() {
    let (store, garden, user) = garden().await;
    let key = TaskKey::new(TaskKind::AddWater, Target::Garden);
    store
        .sync_tasks(garden, &[water_task(t0(), Severity::Critical)], t0())
        .await
        .unwrap();

    let token = store
        .issue_action_grant(user, garden, key.clone(), TaskAction::Complete, t0())
        .await
        .unwrap();

    let grant = store
        .redeem_action_grant(&token, t0())
        .await
        .unwrap()
        .expect("first redemption should succeed");
    assert_eq!(grant.task, key);
    assert_eq!(grant.action, TaskAction::Complete);

    // Replay must fail — these links travel through push relays and lock screens.
    assert!(store.redeem_action_grant(&token, t0()).await.unwrap().is_err());
}

#[tokio::test]
async fn an_unknown_action_link_is_rejected() {
    let (store, _, _) = garden().await;
    let forged = gardyn_auth::SecretToken::generate();
    assert!(store.redeem_action_grant(&forged, t0()).await.unwrap().is_err());
}

#[tokio::test]
async fn deleting_a_garden_takes_its_tasks_and_grants_with_it() {
    let (store, garden, user) = garden().await;
    let key = TaskKey::new(TaskKind::AddWater, Target::Garden);
    store
        .sync_tasks(garden, &[water_task(t0(), Severity::Critical)], t0())
        .await
        .unwrap();
    let token = store
        .issue_action_grant(user, garden, key, TaskAction::Complete, t0())
        .await
        .unwrap();

    store.delete_garden(garden).await.unwrap();

    assert!(store.tasks_for(garden).await.unwrap().is_empty());
    assert!(
        store.redeem_action_grant(&token, t0()).await.unwrap().is_err(),
        "a pending link must not outlive its garden"
    );
}

#[tokio::test]
async fn a_backup_is_a_readable_copy_taken_from_a_live_database() {
    // The real scenario from DESIGN.md: a Proxmox snapshot of a live WAL-mode SQLite
    // file is not guaranteed coherent, so the brain takes a `VACUUM INTO` copy first.
    // Exercised against a file-backed store because that is what gets backed up.
    let dir = std::env::temp_dir();
    let unique = uuid::Uuid::new_v4();
    let source_path = dir.join(format!("gardyn-src-{unique}.db"));
    let backup_path = dir.join(format!("gardyn-backup-{unique}.db"));
    let sqlite_url = |p: &std::path::Path| format!("sqlite://{}", p.to_string_lossy().replace('\\', "/"));

    let store = Store::open(&sqlite_url(&source_path)).await.unwrap();
    let user = store
        .create_user(
            EmailAddress::parse("phil@example.com").unwrap(),
            "Phil",
            "a long enough password",
            t0(),
        )
        .await
        .unwrap();
    let garden = store
        .create_garden("Kitchen", DeviceModel::Studio2, "UTC", user.id, t0())
        .await
        .unwrap();
    store
        .sync_tasks(garden.id, &[water_task(t0(), Severity::Critical)], t0())
        .await
        .unwrap();

    store
        .backup_to(&backup_path.to_string_lossy().replace('\\', "/"))
        .await
        .unwrap();
    assert!(backup_path.exists(), "backup file should have been written");

    // The copy must actually open and contain the data, not merely exist.
    let restored = Store::open(&sqlite_url(&backup_path)).await.unwrap();
    assert_eq!(restored.user_count().await.unwrap(), 1);
    assert_eq!(restored.tasks_for(garden.id).await.unwrap().len(), 1);

    drop(store);
    drop(restored);
    for path in [source_path, backup_path] {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }
}
