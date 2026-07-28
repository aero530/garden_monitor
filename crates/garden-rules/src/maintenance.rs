//! Tank refresh and deep cleaning: the two jobs that are physical work.
//!
//! Everything else this system asks for is a measured dose or a snip. These two are
//! twenty minutes with a towel on the floor, so they get more warning than anything
//! else, and each reminder carries Gardyn's own procedure with it — see
//! [`garden_core::guide`].
//!
//! **The two are scheduled on opposite principles, because Gardyn schedules them on
//! opposite principles.** A refresh is a calendar job: at least every four weeks, with
//! measured symptoms able to pull it forward. A deep clean has no published cadence at
//! all — Gardyn lists conditions ("algae, root pieces, salt deposits, or pests, or if
//! you are planning to take a break from growing"), so the measured signal leads and
//! the calendar is only a backstop against a garden nobody looks at.
//!
//! The Studio 2's sealed "No-Clean Columns" suppress the buildup that drives cleaning
//! on older models, which is why that backstop is loose.

use crate::engine::{PRECEDENCE_FALLBACK, PRECEDENCE_MEASURED, Rule};
use garden_core::{
    Capability, DueWindow, GardenState, PumpBaseline, RuleId, Severity, Target, Task, TaskKind,
};

/// Gardyn's published cadence: "At least every 4 weeks". Their app puts the same number
/// on it from the other direction — logging a refresh "resets your countdown to 28
/// days".
const REFRESH_DUE_DAYS: f64 = 28.0;
/// How far ahead of the due date the reminder appears.
///
/// Gardyn's own widget becomes visible 7 days out, which is about right: long enough to
/// find a free evening and check there is plant food left in the cupboard, short enough
/// that it does not become part of the furniture.
const REFRESH_NOTICE_DAYS: f64 = 7.0;
/// A week past the deadline, where the wording stops being polite.
const REFRESH_LATE_DAYS: f64 = REFRESH_DUE_DAYS + 7.0;
/// How far into a cycle a measured symptom has to be before it is read as tank
/// chemistry rather than as one unhappy plant.
///
/// Gardyn says refresh "sooner if you notice" yellowing, which presumes a tank that has
/// been running a while. Yellowing three days after a refresh is not depletion, and
/// draining a fresh tank would throw away good nutrient and fix nothing.
const REFRESH_EARLY_FLOOR_DAYS: f64 = REFRESH_DUE_DAYS / 2.0;
/// How many slots must be yellowing at once to implicate the tank.
///
/// One chlorotic plant is a plant: wrong variety for the zone, end of its productive
/// life, or a pod that never rooted properly. Several at once, in the same water, is the
/// water.
const CHLOROTIC_SLOTS_FOR_REFRESH: usize = 3;
/// Backstop only. Gardyn publishes no cleaning interval, so this exists to catch a
/// garden that has been running untouched for a year with no fouling sensor fitted.
const DEEP_CLEAN_DUE_DAYS: f64 = 365.0;

pub struct TankRefreshRule;

impl TankRefreshRule {
    pub const ID: RuleId = RuleId::from_static("tank-refresh");

    /// The calendar, plus the one early trigger that needs no measured capability.
    ///
    /// Shared with [`TankRefreshByChlorosisRule`] so the measured rule is a genuine
    /// superset of this one. The engine only lets a higher-precedence rule displace a
    /// lower one when it covers everything the lower one would have said, and the way to
    /// keep that true is to call the same function rather than to reimplement it.
    fn calendar(state: &GardenState) -> Option<(Severity, String)> {
        // An idle device needs no maintenance. Draining a tank with nothing growing in
        // it is busywork, and busywork is what erodes trust in the reminders that matter.
        if state.plantings.is_empty() {
            return None;
        }

        let since = state.tank.days_since_refresh(state.now);
        // No refresh on record reads as infinitely overdue, which is the right ordering
        // but unprintable. Handled first, because every branch below formats `since`.
        if since.is_infinite() {
            return Some((
                Severity::Important,
                "the tank has not been refreshed yet".to_string(),
            ));
        }
        // Heavy algae is its own argument for starting over: topping off a green tank
        // just dilutes it. Available without a vision stage, since the algae reading is
        // simply absent when nothing measures it.
        let algae_urgent = state.algae.is_some_and(|a| a.is_urgent());

        if algae_urgent && since >= REFRESH_EARLY_FLOOR_DAYS {
            return Some((
                Severity::Important,
                format!(
                    "heavy algae with {since:.0} days since the last refresh — refresh \
                     rather than topping off again"
                ),
            ));
        }
        if since >= REFRESH_LATE_DAYS {
            return Some((
                Severity::Important,
                format!(
                    "{since:.0} days since the last tank refresh — {:.0} past Gardyn's \
                     four-week interval",
                    since - REFRESH_DUE_DAYS
                ),
            ));
        }
        if since >= REFRESH_DUE_DAYS {
            return Some((
                Severity::Important,
                format!("{since:.0} days since the last tank refresh — due now"),
            ));
        }
        if since >= REFRESH_DUE_DAYS - REFRESH_NOTICE_DAYS {
            // Advisory, so it rolls up into the daily brief rather than buzzing a
            // phone. It is a heads-up, not an interruption.
            return Some((
                Severity::Advisory,
                format!(
                    "tank refresh due in {:.0} days ({since:.0} days since the last one)",
                    REFRESH_DUE_DAYS - since
                ),
            ));
        }
        None
    }

    /// A window that closes on the published deadline, not `n` days from whenever the
    /// rule happened to fire.
    ///
    /// Fired a week early, the task is due in a week. Fired late, it is due now — which
    /// is what an overdue job is, and pretending otherwise by handing out another
    /// week's grace every evaluation is how a deadline stops meaning anything.
    fn window(state: &GardenState) -> DueWindow {
        let since = state.tank.days_since_refresh(state.now);
        let remaining = if since.is_finite() {
            (REFRESH_DUE_DAYS - since).clamp(1.0, REFRESH_NOTICE_DAYS)
        } else {
            1.0
        };
        DueWindow::within_days(state.now, remaining)
    }
}

impl Rule for TankRefreshRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::TankRefresh]
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        Self::calendar(state)
            .map(|(severity, rationale)| {
                vec![Task::new(
                    TaskKind::TankRefresh,
                    Target::Garden,
                    severity,
                    Self::window(state),
                    rationale,
                    Self::ID,
                )]
            })
            .unwrap_or_default()
    }
}

/// Refresh pulled forward by measured yellowing, with the calendar as backstop.
///
/// Gardyn's guidance is "at least every 4 weeks — sooner if you notice: yellowing
/// leaves, leaf edge burn, yellowing between leaf veins". Those are symptoms of a tank
/// whose nutrient ratios have drifted, and drift is exactly what a refresh fixes. Until
/// Phase A existed there was nothing to notice them with; now `yellowing_index` is
/// measured per slot, so the instruction is actually actionable.
///
/// Interveinal chlorosis and marginal burn are not separable at this resolution, and it
/// does not matter here: all three of Gardyn's symptoms lead to the same action.
pub struct TankRefreshByChlorosisRule;

impl TankRefreshByChlorosisRule {
    pub const ID: RuleId = RuleId::from_static("tank-refresh-by-chlorosis");

    /// Slots whose canopy is yellow enough to count, among slots actually planted.
    ///
    /// Metrics can outlive the planting they measured — a slot harvested yesterday may
    /// still have a reading from last week — and a yellowing empty slot is not evidence
    /// of anything.
    fn chlorotic_slots(state: &GardenState) -> usize {
        state
            .plantings
            .iter()
            .filter(|planting| {
                state
                    .slot_metrics
                    .get(&planting.slot)
                    .is_some_and(|m| m.is_chlorotic())
            })
            .count()
    }
}

impl Rule for TankRefreshByChlorosisRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn requires(&self) -> &'static [Capability] {
        &[Capability::CanopyMetrics]
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::TankRefresh]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_MEASURED
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        if state.plantings.is_empty() {
            return Vec::new();
        }
        let since = state.tank.days_since_refresh(state.now);
        let chlorotic = Self::chlorotic_slots(state);

        let (severity, rationale) =
            if chlorotic >= CHLOROTIC_SLOTS_FOR_REFRESH && since >= REFRESH_EARLY_FLOOR_DAYS {
                (
                    Severity::Important,
                    format!(
                        "{chlorotic} plants yellowing at once with {since:.0} days on this \
                     tank — nutrient ratios have drifted, so refresh early rather than \
                     dosing further"
                    ),
                )
            } else {
                // Nothing measured to act on, so say exactly what the calendar rule would
                // have said. This is what makes displacing it safe.
                match TankRefreshRule::calendar(state) {
                    Some(pair) => pair,
                    None => return Vec::new(),
                }
            };

        vec![Task::new(
            TaskKind::TankRefresh,
            Target::Garden,
            severity,
            TankRefreshRule::window(state),
            rationale,
            Self::ID,
        )]
    }
}

/// The backstop, for a garden with no fouling signal to go on.
///
/// Deliberately weak, and it should stay weak. Gardyn publishes no cleaning interval
/// because elapsed time is not what makes a clean necessary — conditions are. This only
/// exists so a garden running for a year with no pump-current probe eventually gets
/// looked at, and it stays Advisory so it lands in the daily brief rather than buzzing a
/// phone about a job that may not need doing.
pub struct DeepCleanByCalendarRule;

impl DeepCleanByCalendarRule {
    pub const ID: RuleId = RuleId::from_static("deep-clean-by-calendar");

    fn calendar(state: &GardenState) -> Option<(Severity, String)> {
        // An idle device needs no maintenance. Cleaning nothing is busywork, and
        // busywork is what erodes trust in the notifications that do matter.
        if state.plantings.is_empty() {
            return None;
        }
        let since = state.tank.days_since_deep_clean(state.now);
        (since >= DEEP_CLEAN_DUE_DAYS).then(|| {
            // Say what to look for, since the calendar alone has not established that
            // anything is actually wrong. Gardyn's list: algae, root pieces, salt
            // deposits, pests, or a planned break from growing.
            let elapsed = if since.is_infinite() {
                "no deep clean on record".to_string()
            } else {
                format!("{:.0} months since the last deep clean", since / 30.0)
            };
            (
                Severity::Advisory,
                format!(
                    "{elapsed} — check the columns and yPods for algae, root pieces or \
                     salt deposits, and clean if you find them"
                ),
            )
        })
    }
}

impl Rule for DeepCleanByCalendarRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::DeepClean]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_FALLBACK
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        Self::calendar(state)
            .map(|(severity, rationale)| {
                vec![Task::new(
                    TaskKind::DeepClean,
                    Target::Garden,
                    severity,
                    DueWindow::within_days(state.now, 21.0),
                    rationale,
                    Self::ID,
                )]
            })
            .unwrap_or_default()
    }
}

/// Deep clean driven by measured fouling, with the calendar as backstop.
pub struct DeepCleanByFoulingRule;

impl DeepCleanByFoulingRule {
    pub const ID: RuleId = RuleId::from_static("deep-clean-by-fouling");
}

impl Rule for DeepCleanByFoulingRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn requires(&self) -> &'static [Capability] {
        &[Capability::PumpCurrent]
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::DeepClean]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_MEASURED
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        if state.plantings.is_empty() {
            return Vec::new();
        }
        let ratio = state.pump.restriction_ratio();

        let (severity, rationale) = if ratio >= PumpBaseline::URGENT_RATIO {
            (
                Severity::Important,
                format!(
                    "pump drawing {:.0}% above its clean baseline and root pruning has \
                     not brought it down — the lines need clearing",
                    (ratio - 1.0) * 100.0
                ),
            )
        } else {
            match DeepCleanByCalendarRule::calendar(state) {
                Some(pair) => pair,
                None => return Vec::new(),
            }
        };

        vec![Task::new(
            TaskKind::DeepClean,
            Target::Garden,
            severity,
            DueWindow::within_days(state.now, 14.0),
            rationale,
            Self::ID,
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use garden_core::{
        AlgaeReading, Planting, PlantingId, SlotId, SlotMetrics, Timestamp, VarietyId,
        time::add_days,
    };

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn garden(refresh_days_ago: f64) -> GardenState {
        let mut g = GardenState::new_studio_2(t0());
        g.plantings.push(Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new("kale-lacinato"),
            add_days(t0(), -50.0),
        ));
        g.tank.last_refresh = Some(add_days(t0(), -refresh_days_ago));
        g.tank.last_deep_clean = Some(add_days(t0(), -60.0));
        g
    }

    fn refresh_tasks(g: &GardenState) -> Vec<Task> {
        Engine::new(vec![Box::new(TankRefreshRule)])
            .evaluate(g)
            .tasks
    }

    fn clean_engine() -> Engine {
        Engine::new(vec![
            Box::new(DeepCleanByCalendarRule),
            Box::new(DeepCleanByFoulingRule),
        ])
    }

    #[test]
    fn a_recently_refreshed_tank_is_left_alone() {
        assert!(refresh_tasks(&garden(10.0)).is_empty());
    }

    #[test]
    fn refresh_gives_a_weeks_notice_then_escalates() {
        // Gardyn's cadence is four weeks, and their app shows the reminder a week out.
        // Advisory means the daily brief; Important means a push. Nothing before day 21.
        assert!(
            refresh_tasks(&garden(20.0)).is_empty(),
            "too early to mention"
        );

        let notice = &refresh_tasks(&garden(22.0))[0];
        assert_eq!(notice.severity, Severity::Advisory);
        assert!(
            notice.rationale.contains("due in 6 days"),
            "{}",
            notice.rationale
        );

        assert_eq!(
            refresh_tasks(&garden(28.0))[0].severity,
            Severity::Important
        );
        let late = &refresh_tasks(&garden(40.0))[0];
        assert_eq!(late.severity, Severity::Important);
        assert!(late.rationale.contains("12 past"), "{}", late.rationale);
    }

    #[test]
    fn the_due_date_stays_put_as_the_deadline_approaches() {
        // The window closes on the deadline rather than a fixed span from whenever the
        // rule fired. Handing out a fresh week's grace at every evaluation would mean
        // the task was never actually late.
        let early = refresh_tasks(&garden(22.0))[0].due.latest;
        let later = refresh_tasks(&garden(26.0))[0].due.latest;
        assert!(
            later < early,
            "the deadline moved outward: {later} then {early}"
        );

        // Once overdue it is due now, not in another week.
        let overdue = &refresh_tasks(&garden(50.0))[0];
        assert!(
            garden_core::time::days_between(t0(), overdue.due.latest) <= 1.0,
            "an overdue refresh was given more grace"
        );
    }

    #[test]
    fn a_tank_never_refreshed_is_asked_about_without_printing_infinity() {
        let mut g = garden(10.0);
        g.tank.last_refresh = None;
        let tasks = refresh_tasks(&g);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].rationale, "the tank has not been refreshed yet");
        assert!(!tasks[0].rationale.contains("inf"));
    }

    #[test]
    fn heavy_algae_pulls_the_refresh_forward() {
        let mut g = garden(18.0); // cadence alone would be quiet
        g.algae = Some(AlgaeReading {
            at: t0(),
            coverage: 0.4,
        });
        let tasks = refresh_tasks(&g);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].rationale.contains("heavy algae"));
    }

    /// A garden with `count` plantings, `chlorotic` of which are visibly yellowing.
    fn measured(refresh_days_ago: f64, count: u8, chlorotic: u8) -> GardenState {
        let mut g = garden(refresh_days_ago);
        g.plantings.clear();
        g.capabilities.insert(Capability::CanopyMetrics);
        for slot in 0..count {
            g.plantings.push(Planting::new(
                PlantingId(slot as u64 + 1),
                SlotId(slot),
                VarietyId::new("kale-lacinato"),
                add_days(t0(), -40.0),
            ));
            let mut m = SlotMetrics::new(SlotId(slot), t0(), 300.0);
            m.yellowing_index = if slot < chlorotic { 0.5 } else { 0.05 };
            g.slot_metrics.insert(SlotId(slot), m);
        }
        g
    }

    fn refresh_engine() -> Engine {
        Engine::new(vec![
            Box::new(TankRefreshRule),
            Box::new(TankRefreshByChlorosisRule),
        ])
    }

    #[test]
    fn widespread_yellowing_pulls_the_refresh_forward() {
        // Gardyn: "at least every 4 weeks — sooner if you notice yellowing leaves".
        // Halfway through the cycle with three plants yellowing at once is that case.
        let g = measured(16.0, 6, 3);
        let tasks = refresh_engine().evaluate(&g).tasks;
        assert_eq!(tasks.len(), 1, "one refresh task, not two");
        assert_eq!(tasks[0].source, TankRefreshByChlorosisRule::ID);
        assert_eq!(tasks[0].severity, Severity::Important);
        assert!(tasks[0].rationale.contains("3 plants yellowing"));
    }

    #[test]
    fn one_yellowing_plant_is_a_plant_problem_not_a_tank_problem() {
        // A single unhappy plant has a dozen explanations that draining fifteen litres
        // of correctly-dosed water does not fix. Other rules handle the plant.
        assert!(
            refresh_engine()
                .evaluate(&measured(16.0, 6, 1))
                .tasks
                .is_empty()
        );
        assert!(
            refresh_engine()
                .evaluate(&measured(16.0, 6, 2))
                .tasks
                .is_empty()
        );
    }

    #[test]
    fn yellowing_in_a_freshly_refreshed_tank_is_not_blamed_on_the_tank() {
        // Three days in, depletion cannot be the cause, and starting over would throw
        // away good nutrient while fixing nothing.
        assert!(
            refresh_engine()
                .evaluate(&measured(3.0, 6, 5))
                .tasks
                .is_empty()
        );
    }

    #[test]
    fn the_measured_rule_still_says_everything_the_calendar_would_have() {
        // The engine only lets a measured rule displace the calendar when it covers
        // everything the calendar covers. If this regresses, a garden with a camera
        // fitted would silently stop being reminded to refresh at all — strictly worse
        // than having no camera.
        for days in [22.0, 28.0, 40.0, 100.0] {
            let g = measured(days, 6, 0);
            let both = refresh_engine().evaluate(&g).tasks;
            let calendar_only = refresh_tasks(&g);
            assert_eq!(
                both.len(),
                calendar_only.len(),
                "measured and unmeasured disagree at {days} days"
            );
            assert_eq!(
                both[0].severity, calendar_only[0].severity,
                "at {days} days"
            );
            assert_eq!(
                both[0].rationale, calendar_only[0].rationale,
                "at {days} days"
            );
        }
    }

    #[test]
    fn yellowing_in_empty_slots_does_not_count() {
        // Metrics outlive the planting that produced them: a slot harvested yesterday
        // can still carry last week's reading. Counting those would refresh the tank
        // because of plants that are no longer in it.
        let mut g = measured(16.0, 6, 3);
        g.plantings.retain(|p| p.slot.0 >= 3); // remove exactly the yellowing ones
        assert!(refresh_engine().evaluate(&g).tasks.is_empty());
    }

    #[test]
    fn an_empty_garden_is_not_nagged_about_maintenance() {
        let mut g = GardenState::new_studio_2(t0());
        g.tank.last_refresh = Some(add_days(t0(), -200.0));
        assert!(refresh_tasks(&g).is_empty());
    }

    #[test]
    fn deep_clean_is_annual_by_default() {
        let mut g = garden(5.0);
        g.tank.last_deep_clean = Some(add_days(t0(), -400.0));
        let tasks = clean_engine().evaluate(&g).tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].severity, Severity::Advisory);
    }

    #[test]
    fn persistent_fouling_calls_for_a_clean_long_before_the_year_is_up() {
        let mut g = garden(5.0);
        for _ in 0..300 {
            g.pump.observe(600.0, 0.1); // 1.5x baseline
        }
        let tasks = clean_engine().evaluate(&g).tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].severity, Severity::Important);
        assert!(tasks[0].rationale.contains("lines need clearing"));
        assert_eq!(tasks[0].source, DeepCleanByFoulingRule::ID);
    }

    #[test]
    fn a_clean_pump_leaves_only_the_annual_cadence() {
        let g = garden(5.0); // deep clean 60 days ago, pump nominal
        assert!(clean_engine().evaluate(&g).tasks.is_empty());
    }

    #[test]
    fn without_the_pump_sensor_the_calendar_rule_covers_deep_cleaning() {
        let mut g = garden(5.0);
        g.tank.last_deep_clean = Some(add_days(t0(), -400.0));
        g.capabilities.remove(Capability::PumpCurrent);
        let eval = clean_engine().evaluate(&g);
        assert_eq!(eval.tasks[0].source, DeepCleanByCalendarRule::ID);
        assert!(eval.was_suppressed("deep-clean-by-fouling"));
    }
}
