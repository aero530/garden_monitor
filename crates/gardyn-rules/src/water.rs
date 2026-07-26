//! Water level: forecasting a refill before the tank runs dry.

use crate::engine::{PRECEDENCE_FALLBACK, Rule};
use gardyn_core::{
    Capability, DueWindow, GardenState, RuleId, Severity, Target, Task, TaskDetail, TaskKind,
    time::add_days,
};

/// Fraction of capacity that must stay in the tank to keep the pump intake covered.
const RESERVE_FRACTION: f32 = 0.15;

/// Fill fraction that warrants a top-off when no consumption rate is available yet.
const COLD_START_ADVISORY: f32 = 0.35;
const COLD_START_URGENT: f32 = 0.20;

pub struct WaterLevelRule;

impl WaterLevelRule {
    pub const ID: RuleId = RuleId::from_static("water-level");

    /// Map remaining days of water onto how loudly to say so.
    fn severity_for(days_to_reserve: f64) -> Option<Severity> {
        match days_to_reserve {
            d if d <= 0.5 => Some(Severity::Critical),
            d if d <= 1.0 => Some(Severity::Urgent),
            d if d <= 2.0 => Some(Severity::Important),
            d if d <= 4.0 => Some(Severity::Advisory),
            _ => None,
        }
    }
}

impl Rule for WaterLevelRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn requires(&self) -> &'static [Capability] {
        &[Capability::WaterLevel]
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::AddWater]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_FALLBACK
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        let capacity = state.tank_geometry.capacity_l;
        let reserve_l = capacity * RESERVE_FRACTION;
        let volume = state.tank.volume_l;
        let fill_pct = state.fill_fraction() * 100.0;
        let to_add = (capacity - volume).max(0.0);

        // Nothing useful to say about a full tank.
        if to_add < 0.25 {
            return Vec::new();
        }

        let (severity, rationale, due) = match state.tank.days_until(reserve_l) {
            Some(days) => {
                let Some(severity) = Self::severity_for(days) else {
                    return Vec::new();
                };
                let rationale = format!(
                    "tank at {fill_pct:.0}% ({volume:.1} L), using {:.2} L/day — \
                     reserve reached in {days:.1} days",
                    state.tank.consumption_lpd
                );
                let ideal = add_days(state.now, days.max(0.0));
                let latest = state
                    .tank
                    .projected_time_at(0.0, state.now)
                    .unwrap_or_else(|| add_days(state.now, days.max(0.0) + 1.0));
                (
                    severity,
                    rationale,
                    DueWindow::new(state.now, ideal, latest.max(ideal)),
                )
            }
            None => {
                // No usable consumption estimate yet — a fresh install, or the garden
                // has been idle. Fall back to the level alone rather than going silent.
                let fill = state.fill_fraction();
                let severity = if fill <= COLD_START_URGENT {
                    Severity::Urgent
                } else if fill <= COLD_START_ADVISORY {
                    Severity::Advisory
                } else {
                    return Vec::new();
                };
                let rationale = format!(
                    "tank at {fill_pct:.0}% ({volume:.1} L); no consumption history yet, \
                     so this is a level-only estimate"
                );
                (severity, rationale, DueWindow::within_days(state.now, 2.0))
            }
        };

        vec![
            Task::new(
                TaskKind::AddWater,
                Target::Garden,
                severity,
                due,
                rationale,
                Self::ID,
            )
            .with_detail(TaskDetail::Water { litres: to_add }),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use gardyn_core::Timestamp;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn garden(volume_l: f32, consumption_lpd: f32) -> GardenState {
        let mut g = GardenState::new_studio_2(t0());
        g.tank.volume_l = volume_l;
        g.tank.consumption_lpd = consumption_lpd;
        g.sensors.water_level_mm = Some(100.0);
        g
    }

    fn eval(state: &GardenState) -> Vec<Task> {
        Engine::new(vec![Box::new(WaterLevelRule)]).evaluate(state).tasks
    }

    #[test]
    fn a_full_tank_says_nothing() {
        assert!(eval(&garden(15.5, 0.5)).is_empty());
    }

    #[test]
    fn a_comfortable_tank_says_nothing() {
        // 12 L, reserve at 2.325 L, 0.5 L/day => ~19 days. Well outside the window.
        assert!(eval(&garden(12.0, 0.5)).is_empty());
    }

    #[test]
    fn escalation_tracks_days_remaining_not_percentage() {
        // Same 4 L in the tank, different consumption rates, different urgency.
        let slow = eval(&garden(4.0, 0.4)); // ~4.2 days
        let brisk = eval(&garden(4.0, 0.9)); // ~1.9 days
        let fast = eval(&garden(4.0, 2.0)); // ~0.8 days

        assert!(slow.is_empty(), "4+ days out is not yet worth saying");
        assert_eq!(brisk[0].severity, Severity::Important);
        assert_eq!(fast[0].severity, Severity::Urgent);
    }

    #[test]
    fn imminent_dry_out_is_critical() {
        let tasks = eval(&garden(2.5, 3.0));
        assert_eq!(tasks[0].severity, Severity::Critical);
    }

    #[test]
    fn the_task_says_how_much_to_add() {
        let tasks = eval(&garden(4.0, 1.0));
        match tasks[0].detail {
            Some(TaskDetail::Water { litres }) => assert!((litres - 11.5).abs() < 0.01),
            other => panic!("expected a water quantity, got {other:?}"),
        }
    }

    #[test]
    fn rationale_is_specific_enough_to_act_on() {
        let tasks = eval(&garden(4.0, 1.0));
        let r = &tasks[0].rationale;
        assert!(r.contains("26%"), "should state fill percentage: {r}");
        assert!(r.contains("1.00 L/day"), "should state the rate: {r}");
        assert!(r.contains("reserve reached in"), "should forecast: {r}");
    }

    #[test]
    fn cold_start_falls_back_to_level_alone() {
        // No consumption history: still warn, but say the estimate is weaker.
        let tasks = eval(&garden(2.0, 0.0));
        assert_eq!(tasks[0].severity, Severity::Urgent);
        assert!(tasks[0].rationale.contains("no consumption history"));
    }

    #[test]
    fn cold_start_stays_quiet_when_the_tank_is_healthy() {
        assert!(eval(&garden(10.0, 0.0)).is_empty());
    }

    #[test]
    fn without_the_level_sensor_the_rule_does_not_run() {
        let mut g = garden(2.0, 2.0);
        g.capabilities.remove(Capability::WaterLevel);
        let evaluation = Engine::new(vec![Box::new(WaterLevelRule)]).evaluate(&g);
        assert!(evaluation.tasks.is_empty());
        assert!(evaluation.was_suppressed("water-level"));
    }
}
