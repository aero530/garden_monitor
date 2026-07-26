//! Root-zone conditions: water temperature and pH.
//!
//! Both rules produce [`TaskKind::Inspect`], which is a deliberately broad kind. They
//! therefore sit at fallback precedence so they coexist rather than displacing each
//! other, and each tags its task so two unrelated concerns about the same garden do
//! not collapse onto one key.

use crate::engine::Rule;
use gardyn_core::{
    Capability, DueWindow, GardenState, RuleId, Severity, Target, Task, TaskKind,
};

/// Above this, dissolved oxygen falls far enough that root rot becomes likely.
const WARM_ADVISORY_C: f32 = 24.0;
const WARM_URGENT_C: f32 = 27.0;
/// Below this, uptake slows enough to stall growth.
const COLD_ADVISORY_C: f32 = 15.0;

/// Water temperature, from the DS18B20 fitted in the reservoir.
pub struct RootZoneTempRule;

impl RootZoneTempRule {
    pub const ID: RuleId = RuleId::from_static("root-zone-temperature");
    const TAG: &'static str = "water-temp";
}

impl Rule for RootZoneTempRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn requires(&self) -> &'static [Capability] {
        &[Capability::WaterTemperature]
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::Inspect]
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        let Some(temp) = state.sensors.water_temp_c else {
            return Vec::new();
        };
        if state.plantings.is_empty() {
            return Vec::new();
        }

        let (severity, rationale) = if temp >= WARM_URGENT_C {
            (
                Severity::Urgent,
                format!(
                    "reservoir at {temp:.1} °C — warm water holds little dissolved \
                     oxygen and roots will start to rot; cool the room or add chilled water"
                ),
            )
        } else if temp >= WARM_ADVISORY_C {
            (
                Severity::Important,
                format!(
                    "reservoir at {temp:.1} °C, above the {WARM_ADVISORY_C:.0} °C \
                     threshold where root rot risk climbs"
                ),
            )
        } else if temp <= COLD_ADVISORY_C {
            (
                Severity::Advisory,
                format!(
                    "reservoir at {temp:.1} °C — nutrient uptake slows below \
                     {COLD_ADVISORY_C:.0} °C, so growth will lag"
                ),
            )
        } else {
            return Vec::new();
        };

        vec![
            Task::new(
                TaskKind::Inspect,
                Target::Garden,
                severity,
                DueWindow::within_days(state.now, 1.0),
                rationale,
                Self::ID,
            )
            .with_tag(Self::TAG),
        ]
    }
}

/// Solution pH. Inert until a pH probe is fitted.
pub struct PhRule;

impl PhRule {
    pub const ID: RuleId = RuleId::from_static("solution-ph");
    const TAG: &'static str = "ph";

    /// Averaged target band across what is growing.
    fn target(state: &GardenState) -> Option<(f32, f32)> {
        let bands: Vec<_> = state
            .planted()
            .filter_map(|(_, v)| v.ph_target)
            .collect();
        if bands.is_empty() {
            return None;
        }
        let n = bands.len() as f32;
        Some((
            bands.iter().map(|b| b.min).sum::<f32>() / n,
            bands.iter().map(|b| b.max).sum::<f32>() / n,
        ))
    }
}

impl Rule for PhRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn requires(&self) -> &'static [Capability] {
        &[Capability::PotentialHydrogen]
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::Inspect]
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        let (Some(ph), Some((min, max))) = (state.sensors.ph, Self::target(state)) else {
            return Vec::new();
        };
        if ph >= min && ph <= max {
            return Vec::new();
        }

        let (direction, adjust) = if ph < min {
            ("below", "pH up")
        } else {
            ("above", "pH down")
        };
        // Far outside the band, nutrients lock out regardless of how much food is present.
        let severity = if (ph - min).abs() > 1.0 || (ph - max).abs() > 1.0 {
            Severity::Important
        } else {
            Severity::Advisory
        };

        vec![
            Task::new(
                TaskKind::Inspect,
                Target::Garden,
                severity,
                DueWindow::within_days(state.now, 2.0),
                format!(
                    "pH {ph:.1} is {direction} the {min:.1}-{max:.1} band; nutrients \
                     lock out at the extremes, so add {adjust} and re-measure"
                ),
                Self::ID,
            )
            .with_tag(Self::TAG),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use gardyn_core::{Planting, PlantingId, SlotId, Timestamp, VarietyId, time::add_days};

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn garden() -> GardenState {
        let mut g = GardenState::new_studio_2(t0());
        let mut p = Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new("kale-lacinato"),
            add_days(t0(), -50.0),
        );
        p.germinated_at = Some(add_days(t0(), -44.0));
        g.plantings.push(p);
        g.capabilities.insert(Capability::WaterTemperature);
        g
    }

    fn engine() -> Engine {
        Engine::new(vec![Box::new(RootZoneTempRule), Box::new(PhRule)])
    }

    #[test]
    fn a_comfortable_reservoir_says_nothing() {
        let mut g = garden();
        g.sensors.water_temp_c = Some(20.0);
        assert!(engine().evaluate(&g).tasks.is_empty());
    }

    #[test]
    fn warm_water_escalates_in_two_steps() {
        let mut g = garden();
        g.sensors.water_temp_c = Some(25.0);
        assert_eq!(engine().evaluate(&g).tasks[0].severity, Severity::Important);
        g.sensors.water_temp_c = Some(28.0);
        let tasks = engine().evaluate(&g).tasks;
        assert_eq!(tasks[0].severity, Severity::Urgent);
        assert!(tasks[0].rationale.contains("dissolved"));
    }

    #[test]
    fn cold_water_is_advisory_not_alarming() {
        let mut g = garden();
        g.sensors.water_temp_c = Some(12.0);
        let tasks = engine().evaluate(&g).tasks;
        assert_eq!(tasks[0].severity, Severity::Advisory);
        assert!(tasks[0].rationale.contains("uptake slows"));
    }

    #[test]
    fn without_the_probe_the_rule_is_inert() {
        let mut g = garden();
        g.capabilities.remove(Capability::WaterTemperature);
        g.sensors.water_temp_c = Some(30.0);
        let eval = engine().evaluate(&g);
        assert!(eval.tasks.is_empty());
        assert!(eval.was_suppressed("root-zone-temperature"));
    }

    #[test]
    fn the_ph_rule_is_inert_until_the_deferred_probe_arrives() {
        let mut g = garden();
        g.sensors.ph = Some(4.0);
        let eval = engine().evaluate(&g);
        assert!(eval.tasks.is_empty());
        assert!(eval.was_suppressed("solution-ph"));
    }

    #[test]
    fn adding_the_ph_probe_lights_the_rule_up_with_no_other_change() {
        let mut g = garden();
        g.capabilities.insert(Capability::PotentialHydrogen);
        g.sensors.ph = Some(4.2); // band is 5.5-6.5
        let tasks = engine().evaluate(&g).tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].severity, Severity::Important);
        assert!(tasks[0].rationale.contains("pH up"));
    }

    #[test]
    fn alkaline_drift_asks_for_ph_down() {
        let mut g = garden();
        g.capabilities.insert(Capability::PotentialHydrogen);
        g.sensors.ph = Some(7.0);
        assert!(engine().evaluate(&g).tasks[0].rationale.contains("pH down"));
    }

    #[test]
    fn two_unrelated_inspect_concerns_do_not_collapse_into_one() {
        // Both rules target the whole garden with TaskKind::Inspect. Without tagging,
        // one would silently swallow the other.
        let mut g = garden();
        g.capabilities.insert(Capability::PotentialHydrogen);
        g.sensors.water_temp_c = Some(28.0);
        g.sensors.ph = Some(7.5);

        let tasks = engine().evaluate(&g).tasks;
        assert_eq!(tasks.len(), 2, "both concerns must survive");
        assert_ne!(tasks[0].key, tasks[1].key);
    }
}
