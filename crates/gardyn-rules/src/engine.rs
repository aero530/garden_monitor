//! The capability-aware rule engine.
//!
//! # What a rule is
//!
//! A rule is a pure function from a [`GardenState`] snapshot to the set of tasks that
//! *should be outstanding right now*. Rules do not track completion, do not snooze,
//! and do not remember what they emitted last tick — that is the brain's job, keyed
//! off [`TaskKey`]. Keeping rules stateless is what makes it possible to replay a
//! season of recorded history against a modified rule and see what it would have said.
//!
//! # Graceful degradation
//!
//! Each rule declares the capabilities it needs and how authoritative it is. The
//! engine keeps only rules whose requirements are met, then for each [`TaskKind`] runs
//! only the highest-precedence survivor. Fit an EC probe and the volume-estimate
//! dosing rule stands down silently, replaced by the measured one — no code change,
//! no config migration.
//!
//! **A higher-precedence rule must be a superset of the one it displaces.** It wins
//! the whole `TaskKind`, so if it only handles the measured case, the calendar case
//! disappears entirely. Every measured rule here therefore keeps the cadence logic and
//! uses its sensor to escalate or fire early, rather than replacing the logic outright.

use gardyn_core::{Capability, GardenState, RuleId, Task, TaskKey, TaskKind};
use std::collections::BTreeMap;

/// Precedence of a rule that works from the calendar and the variety book alone.
pub const PRECEDENCE_FALLBACK: u8 = 10;
/// Precedence of a rule backed by a real measurement.
pub const PRECEDENCE_MEASURED: u8 = 20;

pub trait Rule: Send + Sync {
    fn id(&self) -> RuleId;

    /// Capabilities without which this rule cannot run.
    fn requires(&self) -> &'static [Capability] {
        &[]
    }

    /// Which task kinds this rule can emit. Used to resolve precedence.
    fn produces(&self) -> &'static [TaskKind];

    fn precedence(&self) -> u8 {
        PRECEDENCE_FALLBACK
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuppressionReason {
    /// The garden does not currently provide what the rule needs.
    MissingCapabilities(Vec<Capability>),
    /// A better-informed rule owns this task kind.
    Outranked { kind: TaskKind, by: RuleId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suppression {
    pub rule: RuleId,
    pub reason: SuppressionReason,
}

impl Suppression {
    /// Operator-facing explanation, for the "why is this rule inactive?" view.
    pub fn explain(&self) -> String {
        match &self.reason {
            SuppressionReason::MissingCapabilities(caps) => {
                let names: Vec<_> = caps.iter().map(|c| c.label()).collect();
                format!("{} needs {}", self.rule, names.join(" and "))
            }
            SuppressionReason::Outranked { kind, by } => {
                format!("{} superseded by {} for '{}'", self.rule, by, kind)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    /// Outstanding tasks, most severe first.
    pub tasks: Vec<Task>,
    /// Rules that ran.
    pub active: Vec<RuleId>,
    /// Rules that did not, and why.
    pub suppressed: Vec<Suppression>,
}

impl Evaluation {
    pub fn tasks_of(&self, kind: TaskKind) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(move |t| t.kind == kind)
    }

    pub fn has(&self, kind: TaskKind) -> bool {
        self.tasks.iter().any(|t| t.kind == kind)
    }

    pub fn was_suppressed(&self, rule: &str) -> bool {
        self.suppressed.iter().any(|s| s.rule.as_str() == rule)
    }
}

pub struct Engine {
    rules: Vec<Box<dyn Rule>>,
}

impl Engine {
    pub fn new(rules: Vec<Box<dyn Rule>>) -> Self {
        Self { rules }
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    pub fn evaluate(&self, state: &GardenState) -> Evaluation {
        let mut suppressed = Vec::new();

        // 1. Drop rules whose hardware or vision stage is not present.
        let mut satisfied: Vec<&dyn Rule> = Vec::new();
        for rule in &self.rules {
            let missing = state.capabilities.missing(rule.requires());
            if missing.is_empty() {
                satisfied.push(rule.as_ref());
            } else {
                suppressed.push(Suppression {
                    rule: rule.id(),
                    reason: SuppressionReason::MissingCapabilities(missing),
                });
            }
        }

        // 2. For each task kind, find the highest precedence any surviving rule offers.
        //
        //    Only a *strictly* higher precedence displaces a rule. Peers at equal
        //    precedence coexist, because they generally cover different situations
        //    within the same kind rather than competing accounts of the same one; any
        //    genuine overlap is collapsed later by key. The representative id is the
        //    lowest at that precedence, so the explanation is deterministic.
        let mut best: BTreeMap<TaskKind, (u8, RuleId)> = BTreeMap::new();
        for rule in &satisfied {
            for kind in rule.produces() {
                let candidate = (rule.precedence(), rule.id());
                best.entry(*kind)
                    .and_modify(|top| {
                        if candidate.0 > top.0 || (candidate.0 == top.0 && candidate.1 < top.1) {
                            *top = candidate.clone();
                        }
                    })
                    .or_insert(candidate);
            }
        }

        // 3. Run the survivors, dropping only kinds that something better-informed owns.
        let mut active = Vec::new();
        let mut collected: Vec<Task> = Vec::new();
        for rule in &satisfied {
            let id = rule.id();
            let lost: Vec<TaskKind> = rule
                .produces()
                .iter()
                .copied()
                .filter(|k| best.get(k).is_some_and(|(p, _)| rule.precedence() < *p))
                .collect();

            for kind in &lost {
                if let Some((_, by)) = best.get(kind) {
                    suppressed.push(Suppression {
                        rule: id.clone(),
                        reason: SuppressionReason::Outranked {
                            kind: *kind,
                            by: by.clone(),
                        },
                    });
                }
            }

            // A rule that lost every kind it produces contributes nothing; skip the work.
            if lost.len() == rule.produces().len() && !rule.produces().is_empty() {
                continue;
            }

            active.push(id.clone());
            collected.extend(
                rule.evaluate(state)
                    .into_iter()
                    .filter(|t| !lost.contains(&t.kind)),
            );
        }

        Evaluation {
            tasks: dedupe_and_rank(collected),
            active,
            suppressed,
        }
    }
}

/// Collapse duplicate keys, keeping the most severe, then order for presentation.
///
/// Two rules can legitimately target the same thing; the operator should see one task,
/// at the highest severity anyone assigned it.
fn dedupe_and_rank(tasks: Vec<Task>) -> Vec<Task> {
    let mut best: BTreeMap<TaskKey, Task> = BTreeMap::new();
    for task in tasks {
        match best.get(&task.key) {
            Some(existing) if existing.severity >= task.severity => {}
            _ => {
                best.insert(task.key.clone(), task);
            }
        }
    }

    let mut out: Vec<Task> = best.into_values().collect();
    out.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(a.due.latest.cmp(&b.due.latest))
            .then(a.key.cmp(&b.key))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gardyn_core::{DueWindow, Severity, Target, Timestamp};

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    struct StubRule {
        id: RuleId,
        requires: &'static [Capability],
        produces: &'static [TaskKind],
        precedence: u8,
        severity: Severity,
    }

    impl Rule for StubRule {
        fn id(&self) -> RuleId {
            self.id.clone()
        }
        fn requires(&self) -> &'static [Capability] {
            self.requires
        }
        fn produces(&self) -> &'static [TaskKind] {
            self.produces
        }
        fn precedence(&self) -> u8 {
            self.precedence
        }
        fn evaluate(&self, state: &GardenState) -> Vec<Task> {
            self.produces
                .iter()
                .map(|k| {
                    Task::new(
                        *k,
                        Target::Garden,
                        self.severity,
                        DueWindow::within_days(state.now, 1.0),
                        format!("from {}", self.id),
                        self.id.clone(),
                    )
                })
                .collect()
        }
    }

    fn fallback() -> Box<dyn Rule> {
        Box::new(StubRule {
            id: RuleId::from_static("estimate"),
            requires: &[],
            produces: &[TaskKind::AddPlantFood],
            precedence: PRECEDENCE_FALLBACK,
            severity: Severity::Advisory,
        })
    }

    fn measured() -> Box<dyn Rule> {
        Box::new(StubRule {
            id: RuleId::from_static("measured"),
            requires: &[Capability::Conductivity],
            produces: &[TaskKind::AddPlantFood],
            precedence: PRECEDENCE_MEASURED,
            severity: Severity::Important,
        })
    }

    #[test]
    fn without_the_probe_the_estimate_rule_runs() {
        let state = GardenState::new_studio_2(t0());
        let engine = Engine::new(vec![fallback(), measured()]);
        let eval = engine.evaluate(&state);

        assert_eq!(eval.active, vec![RuleId::from_static("estimate")]);
        assert!(eval.was_suppressed("measured"));
        assert_eq!(eval.tasks.len(), 1);
        assert_eq!(eval.tasks[0].source, RuleId::from_static("estimate"));
    }

    #[test]
    fn fitting_the_probe_swaps_in_the_measured_rule_with_no_config_change() {
        let mut state = GardenState::new_studio_2(t0());
        state.capabilities.insert(Capability::Conductivity);

        let engine = Engine::new(vec![fallback(), measured()]);
        let eval = engine.evaluate(&state);

        assert_eq!(eval.active, vec![RuleId::from_static("measured")]);
        assert_eq!(eval.tasks.len(), 1);
        assert_eq!(eval.tasks[0].source, RuleId::from_static("measured"));
        // And the estimate rule reports *why* it stood down.
        let s = eval
            .suppressed
            .iter()
            .find(|s| s.rule.as_str() == "estimate")
            .unwrap();
        assert!(matches!(
            s.reason,
            SuppressionReason::Outranked {
                kind: TaskKind::AddPlantFood,
                ..
            }
        ));
    }

    #[test]
    fn losing_the_probe_mid_season_restores_the_fallback() {
        let mut state = GardenState::new_studio_2(t0());
        state.capabilities.insert(Capability::Conductivity);
        let engine = Engine::new(vec![fallback(), measured()]);
        assert_eq!(engine.evaluate(&state).active, vec![RuleId::from_static("measured")]);

        state.capabilities.remove(Capability::Conductivity);
        assert_eq!(engine.evaluate(&state).active, vec![RuleId::from_static("estimate")]);
    }

    #[test]
    fn missing_capability_suppression_explains_itself() {
        let state = GardenState::new_studio_2(t0());
        let engine = Engine::new(vec![measured()]);
        let eval = engine.evaluate(&state);
        let s = &eval.suppressed[0];
        assert_eq!(s.explain(), "measured needs EC probe");
    }

    #[test]
    fn duplicate_keys_collapse_to_the_most_severe() {
        let quiet = Box::new(StubRule {
            id: RuleId::from_static("quiet"),
            requires: &[],
            produces: &[TaskKind::AddWater],
            precedence: PRECEDENCE_FALLBACK,
            severity: Severity::Info,
        });
        let loud = Box::new(StubRule {
            id: RuleId::from_static("loud"),
            requires: &[],
            produces: &[TaskKind::AddWater],
            precedence: PRECEDENCE_FALLBACK,
            severity: Severity::Critical,
        });
        let state = GardenState::new_studio_2(t0());
        // Same precedence, so both run; ties break on id, and "loud" < "quiet".
        let eval = Engine::new(vec![quiet, loud]).evaluate(&state);
        assert_eq!(eval.tasks.len(), 1);
        assert_eq!(eval.tasks[0].severity, Severity::Critical);
    }

    #[test]
    fn tasks_are_ranked_most_severe_first() {
        let state = GardenState::new_studio_2(t0());
        let engine = Engine::new(vec![
            Box::new(StubRule {
                id: RuleId::from_static("a"),
                requires: &[],
                produces: &[TaskKind::Harvest],
                precedence: PRECEDENCE_FALLBACK,
                severity: Severity::Advisory,
            }),
            Box::new(StubRule {
                id: RuleId::from_static("b"),
                requires: &[],
                produces: &[TaskKind::AddWater],
                precedence: PRECEDENCE_FALLBACK,
                severity: Severity::Critical,
            }),
        ]);
        let eval = engine.evaluate(&state);
        assert_eq!(eval.tasks[0].kind, TaskKind::AddWater);
        assert_eq!(eval.tasks[1].kind, TaskKind::Harvest);
    }

    #[test]
    fn evaluation_is_order_independent() {
        let mut state = GardenState::new_studio_2(t0());
        state.capabilities.insert(Capability::Conductivity);
        let forward = Engine::new(vec![fallback(), measured()]).evaluate(&state);
        let reverse = Engine::new(vec![measured(), fallback()]).evaluate(&state);
        assert_eq!(forward.tasks, reverse.tasks);
    }
}
