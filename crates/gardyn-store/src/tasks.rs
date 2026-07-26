//! Task lifecycle: the state the stateless rule engine deliberately does not keep.
//!
//! The rules re-emit whatever should be outstanding on every tick. This module holds
//! what happened to each of those: seen, notified, snoozed, completed, verified.

use crate::{Result, Store, StoreError, ts};
use gardyn_auth::{ActionGrant, ActionGrantId, GrantError, SecretToken, TaskAction, UserId};
use gardyn_core::{GardenId, Severity, Task, TaskKey};
use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

/// How long after "done" a still-emitted task is given before it reopens.
///
/// This is the auto-verification window from the design. Tap "added water", and if
/// the level sensor has not moved by the time this elapses, the task quietly comes
/// back — which is what makes the system trustworthy rather than merely noisy.
pub const VERIFY_WINDOW_MINUTES: f64 = 30.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Open,
    Snoozed,
    Done,
    Dismissed,
}

impl TaskState {
    pub fn slug(self) -> &'static str {
        match self {
            TaskState::Open => "open",
            TaskState::Snoozed => "snoozed",
            TaskState::Done => "done",
            TaskState::Dismissed => "dismissed",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "open" => TaskState::Open,
            "snoozed" => TaskState::Snoozed,
            "done" => TaskState::Done,
            "dismissed" => TaskState::Dismissed,
            other => return Err(StoreError::Corrupt(format!("task state {other:?}"))),
        })
    }
}

/// A task as shown to the operator.
#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub garden: GardenId,
    pub key: TaskKey,
    pub kind: String,
    pub target: String,
    pub severity: Severity,
    pub rationale: String,
    pub detail: Option<String>,
    pub source_rule: String,
    pub first_seen_at: Timestamp,
    pub due_at: Timestamp,
    pub state: TaskState,
    pub snoozed_until: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
    pub completed_by: Option<UserId>,
}

impl TaskRecord {
    /// Whether this needs the operator's attention right now.
    pub fn is_actionable(&self, now: Timestamp) -> bool {
        match self.state {
            TaskState::Open => true,
            TaskState::Snoozed => self.snoozed_until.is_some_and(|until| now >= until),
            TaskState::Done | TaskState::Dismissed => false,
        }
    }

    pub fn is_overdue(&self, now: Timestamp) -> bool {
        self.is_actionable(now) && now > self.due_at
    }
}

fn severity_slug(s: Severity) -> &'static str {
    s.label()
}

fn severity_from_str(s: &str) -> Result<Severity> {
    Ok(match s {
        "info" => Severity::Info,
        "advisory" => Severity::Advisory,
        "important" => Severity::Important,
        "urgent" => Severity::Urgent,
        "critical" => Severity::Critical,
        other => return Err(StoreError::Corrupt(format!("severity {other:?}"))),
    })
}

fn record_from_row(row: &SqliteRow) -> Result<TaskRecord> {
    let garden: String = row.try_get("garden_id")?;
    let severity: String = row.try_get("severity")?;
    let state: String = row.try_get("state")?;
    let completed_by: Option<String> = row.try_get("completed_by")?;

    Ok(TaskRecord {
        garden: GardenId(
            Uuid::parse_str(&garden).map_err(|e| StoreError::Corrupt(format!("garden: {e}")))?,
        ),
        key: TaskKey(row.try_get("task_key")?),
        kind: row.try_get("kind")?,
        target: row.try_get("target")?,
        severity: severity_from_str(&severity)?,
        rationale: row.try_get("rationale")?,
        detail: row.try_get("detail")?,
        source_rule: row.try_get("source_rule")?,
        first_seen_at: ts::decode(&row.try_get::<String, _>("first_seen_at")?)?,
        due_at: ts::decode(&row.try_get::<String, _>("due_at")?)?,
        state: TaskState::parse(&state)?,
        snoozed_until: ts::decode_opt(row.try_get("snoozed_until")?)?,
        completed_at: ts::decode_opt(row.try_get("completed_at")?)?,
        completed_by: completed_by
            .map(|c| {
                Uuid::parse_str(&c)
                    .map(UserId)
                    .map_err(|e| StoreError::Corrupt(format!("completed_by: {e}")))
            })
            .transpose()?,
    })
}

impl Store {
    /// Reconcile a rule evaluation against stored task state.
    ///
    /// Three things happen here, and the order matters:
    ///
    /// 1. Tasks the rules no longer emit are deleted — the underlying condition
    ///    resolved, so there is nothing to act on and nothing to nag about.
    /// 2. New tasks are inserted as open.
    /// 3. A task marked done that the rules are *still* emitting past the
    ///    verification window reopens. That is the closed loop: claiming to have
    ///    watered does not make the tank full.
    pub async fn sync_tasks(
        &self,
        garden: GardenId,
        emitted: &[Task],
        now: Timestamp,
    ) -> Result<SyncOutcome> {
        let mut tx = self.db.begin().await?;
        let mut outcome = SyncOutcome::default();

        let keys: Vec<String> = emitted.iter().map(|t| t.key.0.clone()).collect();

        // 1. Anything not emitted has resolved.
        let existing: Vec<(String,)> =
            sqlx::query_as("SELECT task_key FROM tasks WHERE garden_id = ?1")
                .bind(garden.to_string())
                .fetch_all(&mut *tx)
                .await?;
        for (key,) in existing {
            if !keys.contains(&key) {
                sqlx::query("DELETE FROM tasks WHERE garden_id = ?1 AND task_key = ?2")
                    .bind(garden.to_string())
                    .bind(&key)
                    .execute(&mut *tx)
                    .await?;
                outcome.resolved += 1;
            }
        }

        // 2 and 3.
        for task in emitted {
            let current: Option<SqliteRow> =
                sqlx::query("SELECT * FROM tasks WHERE garden_id = ?1 AND task_key = ?2")
                    .bind(garden.to_string())
                    .bind(&task.key.0)
                    .fetch_optional(&mut *tx)
                    .await?;

            let detail = task.detail.map(|d| d.to_string());

            match current {
                None => {
                    sqlx::query(
                        "INSERT INTO tasks (garden_id, task_key, kind, target, severity,
                            rationale, detail, source_rule, first_seen_at, due_at, state)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'open')",
                    )
                    .bind(garden.to_string())
                    .bind(&task.key.0)
                    .bind(task.kind.label())
                    .bind(task.target.to_string())
                    .bind(severity_slug(task.severity))
                    .bind(&task.rationale)
                    .bind(detail.as_deref())
                    .bind(task.source.as_str())
                    .bind(ts::encode(now))
                    .bind(ts::encode(task.due.latest))
                    .execute(&mut *tx)
                    .await?;
                    outcome.opened += 1;
                }
                Some(row) => {
                    let state = TaskState::parse(&row.try_get::<String, _>("state")?)?;
                    let completed_at = ts::decode_opt(row.try_get("completed_at")?)?;

                    let unverified = state == TaskState::Done
                        && completed_at.is_some_and(|c| {
                            gardyn_core::time::days_between(c, now)
                                > VERIFY_WINDOW_MINUTES / (24.0 * 60.0)
                        });

                    if unverified {
                        sqlx::query(
                            "UPDATE tasks SET state = 'open', completed_at = NULL,
                                completed_by = NULL, severity = ?1, rationale = ?2,
                                detail = ?3, due_at = ?4, notified_at = NULL
                             WHERE garden_id = ?5 AND task_key = ?6",
                        )
                        .bind(severity_slug(task.severity))
                        .bind(&task.rationale)
                        .bind(detail.as_deref())
                        .bind(ts::encode(task.due.latest))
                        .bind(garden.to_string())
                        .bind(&task.key.0)
                        .execute(&mut *tx)
                        .await?;
                        outcome.reopened += 1;
                    } else {
                        // Refresh the wording and urgency; leave lifecycle alone.
                        sqlx::query(
                            "UPDATE tasks SET severity = ?1, rationale = ?2, detail = ?3,
                                due_at = ?4, source_rule = ?5
                             WHERE garden_id = ?6 AND task_key = ?7",
                        )
                        .bind(severity_slug(task.severity))
                        .bind(&task.rationale)
                        .bind(detail.as_deref())
                        .bind(ts::encode(task.due.latest))
                        .bind(task.source.as_str())
                        .bind(garden.to_string())
                        .bind(&task.key.0)
                        .execute(&mut *tx)
                        .await?;
                        outcome.refreshed += 1;
                    }
                }
            }
        }

        tx.commit().await?;
        Ok(outcome)
    }

    pub async fn tasks_for(&self, garden: GardenId) -> Result<Vec<TaskRecord>> {
        let rows = sqlx::query("SELECT * FROM tasks WHERE garden_id = ?1")
            .bind(garden.to_string())
            .fetch_all(&self.db)
            .await?;

        let mut records: Vec<TaskRecord> = rows.iter().map(record_from_row).collect::<Result<_>>()?;
        records.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(a.due_at.cmp(&b.due_at))
                .then(a.key.cmp(&b.key))
        });
        Ok(records)
    }

    pub async fn find_task(&self, garden: GardenId, key: &TaskKey) -> Result<Option<TaskRecord>> {
        let row = sqlx::query("SELECT * FROM tasks WHERE garden_id = ?1 AND task_key = ?2")
            .bind(garden.to_string())
            .bind(&key.0)
            .fetch_optional(&self.db)
            .await?;
        row.as_ref().map(record_from_row).transpose()
    }

    pub async fn complete_task(
        &self,
        garden: GardenId,
        key: &TaskKey,
        by: UserId,
        now: Timestamp,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE tasks SET state = 'done', completed_at = ?1, completed_by = ?2,
                snoozed_until = NULL
             WHERE garden_id = ?3 AND task_key = ?4",
        )
        .bind(ts::encode(now))
        .bind(by.to_string())
        .bind(garden.to_string())
        .bind(&key.0)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn snooze_task(
        &self,
        garden: GardenId,
        key: &TaskKey,
        until: Timestamp,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE tasks SET state = 'snoozed', snoozed_until = ?1
             WHERE garden_id = ?2 AND task_key = ?3",
        )
        .bind(ts::encode(until))
        .bind(garden.to_string())
        .bind(&key.0)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn dismiss_task(&self, garden: GardenId, key: &TaskKey) -> Result<()> {
        sqlx::query(
            "UPDATE tasks SET state = 'dismissed', snoozed_until = NULL
             WHERE garden_id = ?1 AND task_key = ?2",
        )
        .bind(garden.to_string())
        .bind(&key.0)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    // --- One-tap notification links ---------------------------------------------

    pub async fn issue_action_grant(
        &self,
        user: UserId,
        garden: GardenId,
        key: TaskKey,
        action: TaskAction,
        now: Timestamp,
    ) -> Result<SecretToken> {
        let (grant, token) = ActionGrant::issue(user, garden, key, action, now);
        sqlx::query(
            "INSERT INTO action_grants
                (id, user_id, garden_id, task_key, action, digest, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(grant.id.0.to_string())
        .bind(grant.user.to_string())
        .bind(grant.garden.to_string())
        .bind(&grant.task.0)
        .bind(grant.action.slug())
        .bind(grant.digest.as_str())
        .bind(ts::encode(grant.created_at))
        .bind(ts::encode(grant.expires_at))
        .execute(&self.db)
        .await?;
        Ok(token)
    }

    /// Look up and consume a one-tap link.
    ///
    /// Returns the grant so the caller can re-check live membership before acting.
    /// Redemption proves who was sent the link; it does not prove they still have
    /// permission, and those are different questions.
    pub async fn redeem_action_grant(
        &self,
        token: &SecretToken,
        now: Timestamp,
    ) -> Result<std::result::Result<ActionGrant, GrantError>> {
        let row = sqlx::query("SELECT * FROM action_grants WHERE digest = ?1")
            .bind(token.digest().as_str())
            .fetch_optional(&self.db)
            .await?;

        let Some(row) = row else {
            return Ok(Err(GrantError::Expired));
        };

        let action: String = row.try_get("action")?;
        let id: String = row.try_get("id")?;
        let user: String = row.try_get("user_id")?;
        let garden: String = row.try_get("garden_id")?;

        let mut grant = ActionGrant {
            id: ActionGrantId(
                Uuid::parse_str(&id).map_err(|e| StoreError::Corrupt(format!("grant id: {e}")))?,
            ),
            user: UserId(
                Uuid::parse_str(&user).map_err(|e| StoreError::Corrupt(format!("user: {e}")))?,
            ),
            garden: GardenId(
                Uuid::parse_str(&garden).map_err(|e| StoreError::Corrupt(format!("garden: {e}")))?,
            ),
            task: TaskKey(row.try_get("task_key")?),
            action: TaskAction::parse(&action)
                .ok_or_else(|| StoreError::Corrupt(format!("action {action:?}")))?,
            digest: gardyn_auth::TokenDigest::from_stored(row.try_get::<String, _>("digest")?),
            created_at: ts::decode(&row.try_get::<String, _>("created_at")?)?,
            expires_at: ts::decode(&row.try_get::<String, _>("expires_at")?)?,
            used_at: ts::decode_opt(row.try_get("used_at")?)?,
        };

        let expected = grant.task.clone();
        match grant.redeem(&expected, now) {
            Ok(_) => {
                sqlx::query("UPDATE action_grants SET used_at = ?1 WHERE id = ?2")
                    .bind(ts::encode(now))
                    .bind(grant.id.0.to_string())
                    .execute(&self.db)
                    .await?;
                Ok(Ok(grant))
            }
            Err(e) => Ok(Err(e)),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    pub opened: usize,
    pub refreshed: usize,
    pub reopened: usize,
    pub resolved: usize,
}
