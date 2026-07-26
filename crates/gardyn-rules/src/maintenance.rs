//! Tank refresh and deep cleaning.
//!
//! The Studio 2's sealed "No-Clean Columns" suppress the buildup that drives cleaning
//! on older Gardyn models, so the calendar here is deliberately looser than the
//! published guidance for the Home line, and measured fouling is weighted more heavily
//! than elapsed time.

use crate::engine::{PRECEDENCE_FALLBACK, PRECEDENCE_MEASURED, Rule};
use gardyn_core::{
    Capability, DueWindow, GardenState, PumpBaseline, RuleId, Severity, Target, Task, TaskKind,
};

/// Monthly tank refresh keeps water chemistry consistent.
const REFRESH_DUE_DAYS: f64 = 30.0;
const REFRESH_LATE_DAYS: f64 = 40.0;
/// Deep clean is roughly annual on a Studio 2.
const DEEP_CLEAN_DUE_DAYS: f64 = 365.0;

pub struct TankRefreshRule;

impl TankRefreshRule {
    pub const ID: RuleId = RuleId::from_static("tank-refresh");
}

impl Rule for TankRefreshRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::TankRefresh]
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        if state.plantings.is_empty() {
            return Vec::new();
        }

        let since = state.tank.days_since_refresh(state.now);
        let algae_urgent = state.algae.is_some_and(|a| a.is_urgent());

        let (severity, rationale) = if algae_urgent && since >= REFRESH_DUE_DAYS / 2.0 {
            (
                Severity::Important,
                format!(
                    "heavy algae with {since:.0} days since the last refresh — refresh \
                     rather than topping off again"
                ),
            )
        } else if since >= REFRESH_LATE_DAYS {
            (
                Severity::Important,
                format!("{since:.0} days since the last tank refresh"),
            )
        } else if since >= REFRESH_DUE_DAYS {
            (
                Severity::Advisory,
                if since.is_infinite() {
                    "the tank has not been refreshed yet".to_string()
                } else {
                    format!("{since:.0} days since the last tank refresh")
                },
            )
        } else {
            return Vec::new();
        };

        vec![Task::new(
            TaskKind::TankRefresh,
            Target::Garden,
            severity,
            DueWindow::within_days(state.now, 7.0),
            rationale,
            Self::ID,
        )]
    }
}

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
            (
                Severity::Advisory,
                if since.is_infinite() {
                    "no deep clean on record".to_string()
                } else {
                    format!("{:.0} months since the last deep clean", since / 30.0)
                },
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
    use gardyn_core::{
        AlgaeReading, Planting, PlantingId, SlotId, Timestamp, VarietyId, time::add_days,
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
        Engine::new(vec![Box::new(TankRefreshRule)]).evaluate(g).tasks
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
    fn refresh_is_due_monthly_and_escalates_when_late() {
        assert_eq!(refresh_tasks(&garden(32.0))[0].severity, Severity::Advisory);
        assert_eq!(refresh_tasks(&garden(45.0))[0].severity, Severity::Important);
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
