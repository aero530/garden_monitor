//! Tasks: what the system tells the operator to do, and when.

use crate::planting::PlantingId;
use crate::slot::SlotId;
use crate::time::days_between;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

/// Identifies the rule that produced a task. Carried through to the UI so every
/// instruction is traceable to the logic that generated it.
///
/// A `Cow` rather than a `&'static str` so that rule ids survive a round trip through
/// storage: rules declare theirs as compile-time constants, but a task loaded back
/// from the database owns its id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RuleId(pub Cow<'static, str>);

impl RuleId {
    pub const fn from_static(id: &'static str) -> Self {
        RuleId(Cow::Borrowed(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
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
    /// Something needs eyes on it; the system cannot resolve it alone.
    Inspect,
}

impl TaskKind {
    pub fn label(self) -> &'static str {
        match self {
            TaskKind::AddWater => "add water",
            TaskKind::AddPlantFood => "add plant food",
            TaskKind::AddConditioner => "add water conditioner",
            TaskKind::PruneRoots => "prune roots",
            TaskKind::PrunePlant => "prune plant",
            TaskKind::Harvest => "harvest",
            TaskKind::Thin => "thin seedlings",
            TaskKind::Pollinate => "pollinate",
            TaskKind::TankRefresh => "refresh tank",
            TaskKind::DeepClean => "deep clean",
            TaskKind::Replant => "replant slot",
            TaskKind::Inspect => "inspect",
        }
    }

    /// Whether completing this task can be confirmed by a sensor rather than trusted.
    ///
    /// This is what closes the loop: tap "done" on a water task and if the level does
    /// not move within minutes, the task silently reopens.
    pub fn is_sensor_verifiable(self) -> bool {
        matches!(
            self,
            TaskKind::AddWater | TaskKind::TankRefresh | TaskKind::DeepClean
        )
    }

    /// Every kind, for exhaustive iteration.
    pub const ALL: [TaskKind; 12] = [
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
    ];

    /// Recover a kind from its [`label`](Self::label).
    ///
    /// The label is what gets persisted, so this is how a stored task finds its way back
    /// to anything keyed on the enum — the maintenance guides, in particular. Round-trip
    /// with `label` is pinned by a test, since a reworded label would otherwise break the
    /// mapping silently.
    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.label() == label)
    }

    /// The maintenance guide covering this task, if one is published.
    ///
    /// Only the two big physical jobs have one. Dosing tasks carry the amount in the
    /// task itself and need no procedure; a refresh or a clean is twenty minutes of
    /// draining, scrubbing and reassembly that nobody should be recalling from memory.
    /// The slug resolves through [`crate::guide::GuideBook`].
    pub fn guide_slug(self) -> Option<&'static str> {
        match self {
            TaskKind::TankRefresh => Some(crate::guide::TANK_REFRESH),
            TaskKind::DeepClean => Some(crate::guide::DEEP_CLEAN),
            _ => None,
        }
    }
}

impl fmt::Display for TaskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// Applies to the device as a whole.
    Garden,
    Slot(SlotId),
    Planting(PlantingId),
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Garden => f.write_str("garden"),
            Target::Slot(s) => write!(f, "{s}"),
            Target::Planting(p) => write!(f, "planting {}", p.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Worth knowing, never interrupts.
    Info,
    /// Rolls up into the daily brief.
    Advisory,
    /// Push notification.
    Important,
    /// Push plus email.
    Urgent,
    /// Maximum-priority push; bypasses Do Not Disturb.
    Critical,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Advisory => "advisory",
            Severity::Important => "important",
            Severity::Urgent => "urgent",
            Severity::Critical => "critical",
        }
    }

    /// Whether this severity may interrupt outside the daily brief.
    pub fn interrupts(self) -> bool {
        self >= Severity::Important
    }

    /// ntfy priority level. Since SMS was ruled out, priority 5 on the self-hosted
    /// ntfy server is the top of the escalation ladder — it bypasses Do Not Disturb.
    pub fn ntfy_priority(self) -> u8 {
        match self {
            Severity::Info => 1,
            Severity::Advisory => 2,
            Severity::Important => 3,
            Severity::Urgent => 4,
            Severity::Critical => 5,
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// When a task should be done.
///
/// A window rather than an instant, because almost nothing here is a point event.
/// "Top up the tank" is fine any time over three days; the window is what lets the
/// notifier batch sensibly instead of pinging the moment a threshold is crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DueWindow {
    pub earliest: Timestamp,
    pub ideal: Timestamp,
    pub latest: Timestamp,
}

impl DueWindow {
    pub fn new(earliest: Timestamp, ideal: Timestamp, latest: Timestamp) -> Self {
        Self {
            earliest,
            ideal,
            latest,
        }
    }

    /// A window that opens now and closes after `days`.
    pub fn within_days(now: Timestamp, days: f64) -> Self {
        let latest = crate::time::add_days(now, days);
        Self {
            earliest: now,
            ideal: crate::time::add_days(now, days / 2.0),
            latest,
        }
    }

    /// A single instant, for genuinely time-critical work.
    pub fn at(when: Timestamp) -> Self {
        Self {
            earliest: when,
            ideal: when,
            latest: when,
        }
    }

    pub fn is_open(&self, now: Timestamp) -> bool {
        now >= self.earliest
    }

    pub fn is_overdue(&self, now: Timestamp) -> bool {
        now > self.latest
    }

    pub fn days_until_latest(&self, now: Timestamp) -> f64 {
        days_between(now, self.latest)
    }
}

/// Quantified instruction, so the notification can say *how much*, not just *what*.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TaskDetail {
    Water { litres: f32 },
    Dose { millilitres: f32 },
}

impl fmt::Display for TaskDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskDetail::Water { litres } => write!(f, "{litres:.1} L"),
            TaskDetail::Dose { millilitres } => write!(f, "{millilitres:.0} mL"),
        }
    }
}

/// Stable identity for a task, so that re-evaluating every tick updates an existing
/// task rather than creating a duplicate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskKey(pub String);

impl TaskKey {
    pub fn new(kind: TaskKind, target: Target) -> Self {
        Self(format!("{:?}:{}", kind, Self::target_part(target)).to_lowercase())
    }

    /// A key with an extra discriminator.
    ///
    /// Needed for broad kinds like [`TaskKind::Inspect`], where two rules can both
    /// want the operator to look at the garden for unrelated reasons. Without a tag
    /// they would share a key and one concern would silently swallow the other.
    pub fn tagged(kind: TaskKind, target: Target, tag: &str) -> Self {
        Self(format!("{:?}:{}:{tag}", kind, Self::target_part(target)).to_lowercase())
    }

    fn target_part(target: Target) -> String {
        match target {
            Target::Garden => "garden".to_string(),
            Target::Slot(s) => format!("slot:{}", s.0),
            Target::Planting(p) => format!("planting:{}", p.0),
        }
    }
}

impl fmt::Display for TaskKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub key: TaskKey,
    pub kind: TaskKind,
    pub target: Target,
    pub severity: Severity,
    pub due: DueWindow,
    /// Why this task exists, in the operator's terms. Never empty — every
    /// instruction must be able to answer "why am I being told this?".
    pub rationale: String,
    pub detail: Option<TaskDetail>,
    pub source: RuleId,
}

impl Task {
    pub fn new(
        kind: TaskKind,
        target: Target,
        severity: Severity,
        due: DueWindow,
        rationale: impl Into<String>,
        source: RuleId,
    ) -> Self {
        Self {
            key: TaskKey::new(kind, target),
            kind,
            target,
            severity,
            due,
            rationale: rationale.into(),
            detail: None,
            source,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, detail: TaskDetail) -> Self {
        self.detail = Some(detail);
        self
    }

    /// Distinguish this task from others of the same kind and target. See
    /// [`TaskKey::tagged`].
    #[must_use]
    pub fn with_tag(mut self, tag: &str) -> Self {
        self.key = TaskKey::tagged(self.kind, self.target, tag);
        self
    }

    /// One-line rendering for the daily brief and notification body.
    pub fn summary(&self) -> String {
        match self.detail {
            Some(d) => format!("{} ({}) — {}", self.kind, d, self.target),
            None => format!("{} — {}", self.kind, self.target),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::add_days;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    const R: RuleId = RuleId::from_static("test");

    #[test]
    fn every_kind_round_trips_through_its_label() {
        // The label is the persisted form. If a kind stops being recoverable from it —
        // because a label was reworded, or `ALL` was not updated alongside the enum —
        // then a stored maintenance task quietly loses its link to the procedure.
        for kind in TaskKind::ALL {
            assert_eq!(TaskKind::from_label(kind.label()), Some(kind), "{kind:?}");
        }
        assert_eq!(TaskKind::from_label("water the garden"), None);
    }

    #[test]
    fn all_lists_every_variant_exactly_once() {
        let mut labels: Vec<&str> = TaskKind::ALL.iter().map(|k| k.label()).collect();
        let before = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(
            labels.len(),
            before,
            "a label is used twice, or ALL repeats a kind"
        );
    }

    #[test]
    fn only_the_two_physical_jobs_carry_a_guide() {
        // A dose is a number the task already states. A refresh or a clean is a
        // procedure, and those are the two Gardyn actually publishes.
        let with: Vec<TaskKind> = TaskKind::ALL
            .into_iter()
            .filter(|k| k.guide_slug().is_some())
            .collect();
        assert_eq!(with, vec![TaskKind::TankRefresh, TaskKind::DeepClean]);
    }

    #[test]
    fn keys_are_stable_and_distinguish_targets() {
        let a = TaskKey::new(TaskKind::Harvest, Target::Slot(SlotId(3)));
        let b = TaskKey::new(TaskKind::Harvest, Target::Slot(SlotId(3)));
        let c = TaskKey::new(TaskKind::Harvest, Target::Slot(SlotId(4)));
        let d = TaskKey::new(TaskKind::PruneRoots, Target::Slot(SlotId(3)));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    #[test]
    fn severity_orders_and_maps_to_ntfy_priority() {
        assert!(Severity::Critical > Severity::Urgent);
        assert!(Severity::Advisory < Severity::Important);
        assert!(!Severity::Advisory.interrupts());
        assert!(Severity::Important.interrupts());
        assert_eq!(Severity::Critical.ntfy_priority(), 5);
        assert_eq!(Severity::Info.ntfy_priority(), 1);
    }

    #[test]
    fn due_window_tracks_open_and_overdue() {
        let w = DueWindow::within_days(t0(), 4.0);
        assert!(w.is_open(t0()));
        assert!(!w.is_overdue(t0()));
        assert!(w.is_overdue(add_days(t0(), 5.0)));
        assert_eq!(w.days_until_latest(t0()), 4.0);
    }

    #[test]
    fn instant_windows_are_immediately_open_and_expire_at_once() {
        let w = DueWindow::at(t0());
        assert!(w.is_open(t0()));
        assert!(!w.is_overdue(t0()));
        assert!(w.is_overdue(add_days(t0(), 0.01)));
    }

    #[test]
    fn summary_includes_quantity_when_known() {
        let task = Task::new(
            TaskKind::AddWater,
            Target::Garden,
            Severity::Important,
            DueWindow::within_days(t0(), 2.0),
            "tank at 20%",
            R,
        )
        .with_detail(TaskDetail::Water { litres: 4.25 });
        assert_eq!(task.summary(), "add water (4.2 L) — garden");
    }

    #[test]
    fn water_tasks_can_be_verified_by_sensor_but_pruning_cannot() {
        assert!(TaskKind::AddWater.is_sensor_verifiable());
        assert!(!TaskKind::PruneRoots.is_sensor_verifiable());
    }
}
