//! Root pruning.
//!
//! The documented care cycle says "check roots every 2-4 weeks". The INA219 already
//! fitted to the pump turns that into a measurement: current draw rising above its
//! clean baseline means the pump is working against a restriction, and root mass in
//! the flow path is the usual cause.

use crate::engine::{PRECEDENCE_FALLBACK, PRECEDENCE_MEASURED, Rule, RuleScope};
use garden_core::{
    Capability, DueWindow, GardenState, PumpBaseline, RuleId, Severity, Stage, Target, Task,
    TaskKind,
};

/// Gardyn's published cadence: "Checking your plants' roots every 2 weeks".
///
/// Was 21 days, taken as the near end of a documented "2-4 week" window from an older
/// source. The dedicated root article is both more specific and tighter, and it gives
/// the reason: root rot is prevented by catching it early, and "consistent & small
/// root pruning is less likely to shock your plant than more sporadic & severe root
/// pruning". A cadence a week too slow makes every trim a bigger one.
const CHECK_DUE_DAYS: f64 = 14.0;

/// "Check mature plants' roots weekly" — same article, step 7.
///
/// A producing plant has the root mass to fill a yPod, which is when roots start
/// crossing into the pod below and blocking drainage. Only reachable when the stage is
/// known, so simple mode uses the fortnightly figure for everything.
const MATURE_CHECK_DUE_DAYS: f64 = 7.0;

/// Grace between "due" and "overdue".
///
/// One further interval rather than a fixed number of days, so the weekly cadence for
/// mature plants is not treated as late at the same moment as the fortnightly one —
/// which would make every mature plant permanently overdue.
const CHECK_LATE_MULTIPLIER: f64 = 1.5;

/// When flow is restricted, stop waiting for the cadence.
const CHECK_RESTRICTED_DAYS: f64 = 12.0;

/// How often this plant's roots want looking at.
fn due_days(stage: Stage) -> f64 {
    if stage.is_producing() {
        MATURE_CHECK_DUE_DAYS
    } else {
        CHECK_DUE_DAYS
    }
}

/// Plantings old enough for root pruning to be meaningful, with days since last check.
fn candidates(state: &GardenState) -> Vec<(&garden_core::Planting, f64, Stage)> {
    state
        .planted()
        // Seedlings are excluded, which also covers the article's note that "1-2
        // month-old plants are still developing their roots, so it's unlikely you'll
        // need to trim any at that stage".
        .filter(|(p, v)| {
            !matches!(
                p.stage(v, state.now),
                Stage::Seeded | Stage::Seedling | Stage::Spent
            )
        })
        .map(|(p, v)| {
            (
                p,
                p.days_since_root_check(state.now),
                p.stage(v, state.now),
            )
        })
        .collect()
}

/// The same cadence, asked of the garden rather than of each plant.
///
/// Simple mode has no plantings to iterate, and the answer is a single timestamp:
/// when the reservoir was last looked into. The severities and the wording are shared
/// with the per-plant path deliberately — the job is identical, only the thing being
/// counted differs, and two sets of thresholds would drift.
fn garden_root_check(state: &GardenState, source: RuleId, urgent_after: f64) -> Vec<Task> {
    let since = state
        .tank
        .last_root_check
        .map_or(f64::INFINITY, |at| garden_core::time::days_between(at, state.now));

    let severity = if since >= urgent_after * CHECK_LATE_MULTIPLIER {
        Severity::Important
    } else if since >= urgent_after {
        Severity::Advisory
    } else {
        return Vec::new();
    };
    let rationale = if since.is_infinite() {
        "roots have never been checked".to_string()
    } else {
        format!("{since:.0} days since the last root check")
    };

    vec![Task::new(
        TaskKind::PruneRoots,
        Target::Garden,
        severity,
        DueWindow::within_days(state.now, 3.0),
        rationale,
        source,
    )]
}

fn task(
    planting: &garden_core::Planting,
    severity: Severity,
    rationale: String,
    state: &GardenState,
    source: RuleId,
) -> Task {
    Task::new(
        TaskKind::PruneRoots,
        Target::Planting(planting.id),
        severity,
        DueWindow::within_days(state.now, 7.0),
        rationale,
        source,
    )
}

/// Fixed cadence from the care cycle.
pub struct RootPruneCadenceRule;

impl RootPruneCadenceRule {
    pub const ID: RuleId = RuleId::from_static("root-prune-cadence");
}

impl Rule for RootPruneCadenceRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn scope(&self) -> RuleScope {
        RuleScope::Garden
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::PruneRoots]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_FALLBACK
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        if !state.mode.tracks_plants() {
            return garden_root_check(state, Self::ID, CHECK_DUE_DAYS);
        }
        candidates(state)
            .into_iter()
            .filter_map(|(planting, since, stage)| {
                let due = due_days(stage);
                let severity = if since >= due * CHECK_LATE_MULTIPLIER {
                    Severity::Important
                } else if since >= due {
                    Severity::Advisory
                } else {
                    return None;
                };
                let rationale = if since.is_infinite() {
                    "roots have never been checked".to_string()
                } else {
                    format!("{since:.0} days since the last root check")
                };
                Some(task(planting, severity, rationale, state, Self::ID))
            })
            .collect()
    }
}

/// Cadence, plus early firing when the pump reports a restriction.
pub struct RootPruneByFlowRule;

impl RootPruneByFlowRule {
    pub const ID: RuleId = RuleId::from_static("root-prune-by-flow");
}

impl Rule for RootPruneByFlowRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn requires(&self) -> &'static [Capability] {
        &[Capability::PumpCurrent]
    }

    fn scope(&self) -> RuleScope {
        RuleScope::Garden
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::PruneRoots]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_MEASURED
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        // `None` means nothing has caught the pump running yet, which reads the same
        // as "not restricted" on purpose: this rule owns the kind, so standing down
        // silently would take root pruning off the list entirely.
        let ratio = state.pump.restriction_ratio();
        let restricted = ratio.is_some_and(|r| r >= PumpBaseline::ADVISORY_RATIO);

        // Not restricted: this rule owns the kind, so fall through to plain cadence.
        if !restricted {
            return RootPruneCadenceRule
                .evaluate(state)
                .into_iter()
                .map(|mut t| {
                    t.source = Self::ID;
                    t
                })
                .collect();
        }

        let ratio = ratio.expect("restricted implies a measurement");
        let excess = (ratio - 1.0) * 100.0;
        let urgent = ratio >= PumpBaseline::URGENT_RATIO;

        // Same early trigger, aimed at the tower. The pump measures the whole flow
        // path, so a restriction it sees was never attributable to one plant anyway.
        if !state.mode.tracks_plants() {
            let mut tasks = garden_root_check(state, Self::ID, CHECK_RESTRICTED_DAYS);
            for t in &mut tasks {
                t.rationale =
                    format!("pump drawing {excess:.0}% above its clean baseline — check the roots");
                if urgent {
                    t.severity = Severity::Urgent;
                }
            }
            return tasks;
        }

        candidates(state)
            .into_iter()
            .filter_map(|(planting, since, _)| {
                let due_at = if urgent { 0.0 } else { CHECK_RESTRICTED_DAYS };
                if since < due_at {
                    return None;
                }
                let severity = if urgent {
                    Severity::Urgent
                } else {
                    Severity::Important
                };
                let last = if since.is_infinite() {
                    "never checked".to_string()
                } else {
                    format!("last checked {since:.0} days ago")
                };
                let rationale = format!(
                    "pump drawing {excess:.0}% above its clean baseline, which points to \
                     flow restriction; {last}"
                );
                Some(task(planting, severity, rationale, state, Self::ID))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use garden_core::{Planting, PlantingId, SlotId, Timestamp, VarietyId, time::add_days};

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn garden(last_check_days_ago: Option<f64>) -> GardenState {
        garden_aged(last_check_days_ago, 30.0)
    }

    /// A garden holding one kale, germinated `germinated_days_ago` ago.
    ///
    /// The age is the knob because it decides the stage, and the stage decides the
    /// cadence: producing plants are checked weekly, everything else fortnightly.
    fn garden_aged(last_check_days_ago: Option<f64>, germinated_days_ago: f64) -> GardenState {
        let mut g = GardenState::new_studio_2(t0());
        let mut p = Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new("kale-lacinato"),
            add_days(t0(), -(germinated_days_ago + 6.0)),
        );
        p.germinated_at = Some(add_days(t0(), -germinated_days_ago));
        p.last_root_check = last_check_days_ago.map(|d| add_days(t0(), -d));
        g.plantings.push(p);
        g
    }

    /// The stage the fixture actually reached, so these tests cannot quietly drift
    /// onto the wrong cadence if the variety book is revised.
    fn stage_of(g: &GardenState) -> Stage {
        let (p, v) = g.planted().next().expect("one planting");
        p.stage(v, g.now)
    }

    fn both_rules() -> Engine {
        Engine::new(vec![
            Box::new(RootPruneCadenceRule),
            Box::new(RootPruneByFlowRule),
        ])
    }

    #[test]
    fn recently_checked_roots_are_left_alone() {
        assert!(both_rules().evaluate(&garden(Some(5.0))).tasks.is_empty());
    }

    #[test]
    fn a_producing_plant_is_asked_about_weekly() {
        // "Check mature plants' roots weekly" — the root article, step 7.
        let g = garden_aged(Some(8.0), 70.0);
        assert!(stage_of(&g).is_producing(), "fixture should be producing");

        let tasks = both_rules().evaluate(&g).tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].severity, Severity::Advisory);

        // And six days is inside the week.
        assert!(both_rules().evaluate(&garden_aged(Some(6.0), 70.0)).tasks.is_empty());
    }

    #[test]
    fn a_plant_not_yet_producing_is_asked_about_fortnightly() {
        // "Checking your plants' roots every 2 weeks" — the same article's prevention
        // list. Eight days is due for a mature plant and not for this one, which is
        // the whole reason the cadence varies by stage.
        let g = garden_aged(Some(8.0), 30.0);
        assert!(!stage_of(&g).is_producing(), "fixture should not be producing yet");
        assert!(both_rules().evaluate(&g).tasks.is_empty());

        let due = garden_aged(Some(15.0), 30.0);
        assert_eq!(both_rules().evaluate(&due).tasks.len(), 1);
    }

    #[test]
    fn overdue_escalates_at_half_an_interval_past_due() {
        // Proportional, not a fixed number of days. A flat grace measured for the
        // fortnightly cadence would make every mature plant overdue the moment it
        // became due, since its whole interval is shorter than that grace.
        let mature = both_rules().evaluate(&garden_aged(Some(11.0), 70.0)).tasks;
        assert_eq!(mature[0].severity, Severity::Important);

        let young = both_rules().evaluate(&garden_aged(Some(11.0), 30.0)).tasks;
        assert!(young.is_empty(), "11 days is not yet due on the fortnightly cadence");
        let young_late = both_rules().evaluate(&garden_aged(Some(22.0), 30.0)).tasks;
        assert_eq!(young_late[0].severity, Severity::Important);
    }

    #[test]
    fn the_cadence_matches_the_article_it_came_from() {
        // The numbers and the prose have to agree, or the guide linked from the
        // reminder contradicts the reminder.
        assert_eq!(CHECK_DUE_DAYS, 14.0);
        assert_eq!(MATURE_CHECK_DUE_DAYS, 7.0);

        let guide = garden_core::GuideBook::published()
            .get(garden_core::guide::ROOT_CHECK)
            .expect("the root guide is embedded")
            .clone();
        let prose: String = guide
            .sections
            .iter()
            .flat_map(|s| std::iter::once(s.title.clone()).chain(s.body.iter().cloned()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(prose.contains("every 2 weeks"), "{prose}");
        assert!(prose.contains("weekly"), "{prose}");
    }

    #[test]
    fn a_never_checked_planting_is_flagged_in_plain_language() {
        let tasks = both_rules().evaluate(&garden(None)).tasks;
        assert!(tasks[0].rationale.contains("never been checked"));
    }

    #[test]
    fn seedlings_are_not_asked_to_have_their_roots_pruned() {
        let mut g = GardenState::new_studio_2(t0());
        let mut p = Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new("kale-lacinato"),
            add_days(t0(), -10.0),
        );
        p.germinated_at = Some(add_days(t0(), -4.0)); // seedling
        g.plantings.push(p);
        assert!(both_rules().evaluate(&g).tasks.is_empty());
    }

    #[test]
    fn a_restricted_pump_fires_well_before_the_cadence_would() {
        // 13 days: past the restricted threshold of 12, inside the fortnightly cadence
        // of 14. A narrow gap now that the cadence has tightened, and that narrowness
        // is the point — the flow signal buys less time than it used to.
        let mut g = garden(Some(13.0));
        // Stated, not inherited. These used to lean on a hardcoded 400 mA default
        // that no real garden ever had, which is how the restriction percentage came
        // to be measured against a number nobody had measured.
        g.pump.nominal_ma = Some(400.0);
        for _ in 0..300 {
            g.pump.observe(500.0, 0.1); // 1.25x
        }
        let tasks = both_rules().evaluate(&g).tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].severity, Severity::Important);
        assert!(tasks[0].rationale.contains("above its clean baseline"));
        assert_eq!(tasks[0].source, RootPruneByFlowRule::ID);
    }

    #[test]
    fn severe_restriction_is_urgent_regardless_of_when_roots_were_last_checked() {
        let mut g = garden(Some(1.0));
        g.pump.nominal_ma = Some(400.0);
        for _ in 0..300 {
            g.pump.observe(600.0, 0.1); // 1.5x
        }
        let tasks = both_rules().evaluate(&g).tasks;
        assert_eq!(tasks[0].severity, Severity::Urgent);
    }

    #[test]
    fn with_a_clean_pump_the_measured_rule_still_honours_the_cadence() {
        // The higher-precedence rule owns the kind, so it must not drop the calendar case.
        let g = garden(Some(30.0));
        let eval = both_rules().evaluate(&g);
        assert_eq!(eval.tasks.len(), 1);
        assert_eq!(eval.tasks[0].severity, Severity::Important);
        assert!(eval.was_suppressed("root-prune-cadence"));
    }

    #[test]
    fn without_the_pump_sensor_the_cadence_rule_takes_over() {
        let mut g = garden(Some(30.0));
        g.capabilities.remove(Capability::PumpCurrent);
        let eval = both_rules().evaluate(&g);
        assert_eq!(eval.tasks.len(), 1);
        assert_eq!(eval.tasks[0].source, RootPruneCadenceRule::ID);
        assert!(eval.was_suppressed("root-prune-by-flow"));
    }
}
