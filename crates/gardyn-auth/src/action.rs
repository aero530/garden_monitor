//! One-tap links in notifications.
//!
//! A push notification carries Done / Snooze / Not-applicable buttons that work
//! without signing in — that is the entire point of the "I don't want to have to
//! remember" requirement. It is also the most dangerous thing in the system, because
//! a link in a notification travels through push servers and lock screens.
//!
//! So a grant is narrow by construction: one user, one garden, one task, one action,
//! one use, short-lived, and **not a session**. Redeeming it lets you tick off that
//! task and nothing else.

use crate::role::Permission;
use crate::token::{SecretToken, TokenDigest};
use crate::user::UserId;
use gardyn_core::{GardenId, TaskKey};
use gardyn_core::time::add_days;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Grants stay usable a little longer than the notification is likely to matter, so
/// a link tapped the next morning still works.
pub const DEFAULT_GRANT_LIFETIME_DAYS: f64 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAction {
    Complete,
    Snooze,
    Dismiss,
}

impl TaskAction {
    pub fn label(self) -> &'static str {
        match self {
            TaskAction::Complete => "Done",
            TaskAction::Snooze => "Snooze",
            TaskAction::Dismiss => "Not applicable",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            TaskAction::Complete => "complete",
            TaskAction::Snooze => "snooze",
            TaskAction::Dismiss => "dismiss",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "complete" => Some(TaskAction::Complete),
            "snooze" => Some(TaskAction::Snooze),
            "dismiss" => Some(TaskAction::Dismiss),
            _ => None,
        }
    }

    /// All three are task bookkeeping, so all three need the same permission.
    pub fn required_permission(self) -> Permission {
        Permission::CompleteTask
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActionGrantId(pub Uuid);

impl ActionGrantId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ActionGrantId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionGrant {
    pub id: ActionGrantId,
    /// The one person this link was sent to.
    pub user: UserId,
    pub garden: GardenId,
    pub task: TaskKey,
    pub action: TaskAction,
    pub digest: TokenDigest,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub used_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    #[error("that link has expired — open the app instead")]
    Expired,
    #[error("that link has already been used")]
    AlreadyUsed,
    #[error("that link is for a different task")]
    WrongTask,
}

impl ActionGrant {
    pub fn issue(
        user: UserId,
        garden: GardenId,
        task: TaskKey,
        action: TaskAction,
        now: Timestamp,
    ) -> (Self, SecretToken) {
        let token = SecretToken::generate();
        let grant = Self {
            id: ActionGrantId::new(),
            user,
            garden,
            task,
            action,
            digest: token.digest(),
            created_at: now,
            expires_at: add_days(now, DEFAULT_GRANT_LIFETIME_DAYS),
            used_at: None,
        };
        (grant, token)
    }

    /// Consume the grant for a specific task.
    ///
    /// `expected` is passed in from the request path rather than trusted from the
    /// grant, so that a valid link for a harmless task cannot be replayed against a
    /// different one.
    pub fn redeem(
        &mut self,
        expected: &TaskKey,
        now: Timestamp,
    ) -> Result<TaskAction, GrantError> {
        if self.used_at.is_some() {
            return Err(GrantError::AlreadyUsed);
        }
        if now >= self.expires_at {
            return Err(GrantError::Expired);
        }
        if &self.task != expected {
            return Err(GrantError::WrongTask);
        }
        self.used_at = Some(now);
        Ok(self.action)
    }

    pub fn is_usable(&self, now: Timestamp) -> bool {
        self.used_at.is_none() && now < self.expires_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gardyn_core::{Target, TaskKind};

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn key() -> TaskKey {
        TaskKey::new(TaskKind::AddWater, Target::Garden)
    }

    fn grant() -> (ActionGrant, SecretToken) {
        ActionGrant::issue(
            UserId::new(),
            GardenId::new(),
            key(),
            TaskAction::Complete,
            t0(),
        )
    }

    #[test]
    fn a_fresh_grant_redeems_once() {
        let (mut g, _) = grant();
        assert!(g.is_usable(t0()));
        assert_eq!(g.redeem(&key(), t0()).unwrap(), TaskAction::Complete);
    }

    #[test]
    fn a_grant_cannot_be_replayed() {
        // Notification links end up in logs, lock screens, and push relays.
        let (mut g, _) = grant();
        g.redeem(&key(), t0()).unwrap();
        assert_eq!(g.redeem(&key(), t0()), Err(GrantError::AlreadyUsed));
        assert!(!g.is_usable(t0()));
    }

    #[test]
    fn a_grant_cannot_be_pointed_at_a_different_task() {
        // The dangerous case: a link for "add water" replayed against "deep clean".
        let (mut g, _) = grant();
        let other = TaskKey::new(TaskKind::DeepClean, Target::Garden);
        assert_eq!(g.redeem(&other, t0()), Err(GrantError::WrongTask));
        // ...and the failed attempt must not burn the legitimate grant.
        assert!(g.is_usable(t0()));
        assert!(g.redeem(&key(), t0()).is_ok());
    }

    #[test]
    fn grants_expire() {
        let (mut g, _) = grant();
        let late = gardyn_core::time::add_days(t0(), DEFAULT_GRANT_LIFETIME_DAYS + 0.1);
        assert!(!g.is_usable(late));
        assert_eq!(g.redeem(&key(), late), Err(GrantError::Expired));
    }

    #[test]
    fn an_expired_grant_reports_expiry_rather_than_reuse() {
        let (mut g, _) = grant();
        let late = gardyn_core::time::add_days(t0(), 10.0);
        assert_eq!(g.redeem(&key(), late), Err(GrantError::Expired));
    }

    #[test]
    fn the_link_secret_is_not_stored() {
        let (g, token) = grant();
        assert_ne!(g.digest.as_str(), token.expose());
        assert_eq!(g.digest, token.digest());
    }

    #[test]
    fn actions_round_trip_through_urls() {
        for action in [TaskAction::Complete, TaskAction::Snooze, TaskAction::Dismiss] {
            assert_eq!(TaskAction::parse(action.slug()), Some(action));
        }
        assert_eq!(TaskAction::parse("delete-garden"), None);
    }

    #[test]
    fn every_action_still_requires_task_permission() {
        // A caretaker can tick tasks; a viewer holding a link cannot.
        for action in [TaskAction::Complete, TaskAction::Snooze, TaskAction::Dismiss] {
            assert_eq!(action.required_permission(), Permission::CompleteTask);
        }
    }
}
