//! Plant food and water conditioner.
//!
//! This is the clearest illustration of why the capability model exists. Without an
//! EC probe the best we can do is dose in proportion to the water added — an open-loop
//! estimate that drifts. With one, the same task kind becomes a measurement against a
//! per-variety target. [`PlantFoodByEcRule`] deliberately keeps the volume logic as
//! its own fallback path, because winning a `TaskKind` means owning every case of it.

use crate::engine::{PRECEDENCE_FALLBACK, PRECEDENCE_MEASURED, Rule};
use garden_core::{
    Capability, DueWindow, GardenState, RuleId, Severity, Stage, Target, Task, TaskDetail, TaskKind,
};

/// Water added since the last dose that justifies feeding again.
const TRIGGER_LITRES: f32 = 1.5;
/// Beyond this the solution is meaningfully dilute, not just drifting.
const URGENT_LITRES: f32 = 5.0;

/// Strength to dose at, given what is growing.
///
/// Food during germination hinders it, so a garden of un-sprouted cubes gets nothing.
/// A garden of seedlings gets the reduced "sprout dose". Only once something is
/// actually producing does the tank run at full strength.
fn dose_fraction(state: &GardenState) -> f32 {
    let mut any = false;
    let mut any_past_seedling = false;
    let mut any_germinated = false;

    for (planting, variety) in state.planted() {
        any = true;
        match planting.stage(variety, state.now) {
            Stage::Seeded => {}
            Stage::Seedling => any_germinated = true,
            _ => {
                any_germinated = true;
                any_past_seedling = true;
            }
        }
    }

    if !any || !any_germinated {
        0.0
    } else if any_past_seedling {
        1.0
    } else {
        state.dosing.sprout_dose_fraction
    }
}

fn severity_for_litres(litres: f32) -> Severity {
    if litres >= URGENT_LITRES {
        Severity::Important
    } else {
        Severity::Advisory
    }
}

/// Open-loop dosing: food in proportion to water added since the last dose.
pub struct PlantFoodByVolumeRule;

impl PlantFoodByVolumeRule {
    pub const ID: RuleId = RuleId::from_static("plant-food-by-volume");
}

impl Rule for PlantFoodByVolumeRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::AddPlantFood]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_FALLBACK
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        let added = state.tank.litres_added_since_food_dose;
        if added < TRIGGER_LITRES {
            return Vec::new();
        }

        let fraction = dose_fraction(state);
        if fraction <= 0.0 {
            return Vec::new();
        }

        let ml = added * state.dosing.food_ml_per_litre * fraction;
        let strength = if fraction < 1.0 {
            format!(" at {:.0}% sprout strength", fraction * 100.0)
        } else {
            String::new()
        };
        let rationale = format!(
            "{added:.1} L added since the last dose{strength}; \
             no EC probe fitted, so this is a volume estimate"
        );

        vec![
            Task::new(
                TaskKind::AddPlantFood,
                Target::Garden,
                severity_for_litres(added),
                DueWindow::within_days(state.now, 3.0),
                rationale,
                Self::ID,
            )
            .with_detail(TaskDetail::Dose { millilitres: ml }),
        ]
    }
}

/// Closed-loop dosing against a measured conductivity target.
pub struct PlantFoodByEcRule;

impl PlantFoodByEcRule {
    pub const ID: RuleId = RuleId::from_static("plant-food-by-ec");

    /// Target band, averaged across what is actually growing.
    ///
    /// A mixed tank cannot satisfy leafy greens and fruiting plants simultaneously —
    /// their bands do not overlap — so the average is a genuine compromise. The
    /// succession planner should eventually avoid pairing them; until then, aim for
    /// the middle and say so.
    fn target(state: &GardenState) -> Option<(f32, f32, bool)> {
        let mut mins = Vec::new();
        let mut maxes = Vec::new();
        for (_, variety) in state.planted() {
            if let Some(range) = variety.ec_target {
                mins.push(range.min);
                maxes.push(range.max);
            }
        }
        if mins.is_empty() {
            return None;
        }
        let n = mins.len() as f32;
        let min = mins.iter().sum::<f32>() / n;
        let max = maxes.iter().sum::<f32>() / n;
        // Bands conflict when the neediest variety wants more than the leanest tolerates.
        let conflicted = mins.iter().cloned().fold(f32::MIN, f32::max)
            > maxes.iter().cloned().fold(f32::MAX, f32::min);
        Some((min, max, conflicted))
    }
}

impl Rule for PlantFoodByEcRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn requires(&self) -> &'static [Capability] {
        &[Capability::Conductivity]
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::AddPlantFood]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_MEASURED
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        // This rule owns AddPlantFood outright, so it must still handle the case where
        // the reading is momentarily absent.
        let (Some(ec), Some((min, max, conflicted))) =
            (state.sensors.ec_ms_cm, Self::target(state))
        else {
            return PlantFoodByVolumeRule.evaluate(state);
        };

        if ec >= min {
            return Vec::new();
        }

        let fraction = dose_fraction(state);
        if fraction <= 0.0 {
            return Vec::new();
        }

        let midpoint = (min + max) / 2.0;
        let deficit = midpoint - ec;
        let ml = state
            .dosing
            .food_ml_for_ec_delta(deficit, state.tank.volume_l)
            * fraction;

        let severity = if ec < min * 0.7 {
            Severity::Urgent
        } else {
            Severity::Important
        };
        let mut rationale = format!(
            "EC {ec:.2} mS/cm is below the {min:.2}-{max:.2} band for what is growing; \
             dosing to {midpoint:.2}"
        );
        if conflicted {
            rationale.push_str(
                " (note: leafy and fruiting varieties in the same tank want different \
                 strengths, so this target is a compromise)",
            );
        }

        vec![
            Task::new(
                TaskKind::AddPlantFood,
                Target::Garden,
                severity,
                DueWindow::within_days(state.now, 2.0),
                rationale,
                Self::ID,
            )
            .with_detail(TaskDetail::Dose { millilitres: ml }),
        ]
    }
}

/// Conditioner goes in with every top-off and refresh.
pub struct ConditionerRule;

impl ConditionerRule {
    pub const ID: RuleId = RuleId::from_static("conditioner-cadence");
    /// Longest tolerable gap between doses, independent of top-offs.
    const MAX_GAP_DAYS: f64 = 9.0;
}

impl Rule for ConditionerRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::AddConditioner]
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        if state.plantings.is_empty() {
            return Vec::new();
        }

        let since = state.tank.days_since_conditioner(state.now);
        // Water went in more recently than conditioner did.
        let owed_for_top_off = match (state.tank.last_top_off, state.tank.last_conditioner) {
            (Some(top_off), Some(cond)) => top_off > cond,
            (Some(_), None) => true,
            _ => false,
        };

        let (severity, rationale) = if since > Self::MAX_GAP_DAYS {
            (
                Severity::Advisory,
                format!("{since:.0} days since the last conditioner dose"),
            )
        } else if owed_for_top_off {
            (
                Severity::Advisory,
                "water was topped off without conditioner".to_string(),
            )
        } else {
            return Vec::new();
        };

        let ml = state.tank.volume_l * state.dosing.conditioner_ml_per_litre;
        vec![
            Task::new(
                TaskKind::AddConditioner,
                Target::Garden,
                severity,
                DueWindow::within_days(state.now, 4.0),
                rationale,
                Self::ID,
            )
            .with_detail(TaskDetail::Dose { millilitres: ml }),
        ]
    }
}

/// Visible algae pulls the conditioner dose forward and raises its priority.
pub struct ConditionerByAlgaeRule;

impl ConditionerByAlgaeRule {
    pub const ID: RuleId = RuleId::from_static("conditioner-by-algae");
}

impl Rule for ConditionerByAlgaeRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn requires(&self) -> &'static [Capability] {
        &[Capability::CanopyMetrics]
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::AddConditioner]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_MEASURED
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        // Owns the kind, so the cadence case still has to be covered.
        let mut tasks = ConditionerRule.evaluate(state);

        if let Some(algae) = state.algae.filter(|a| a.is_advisory()) {
            let severity = if algae.is_urgent() {
                Severity::Important
            } else {
                Severity::Advisory
            };
            let ml = state.tank.volume_l * state.dosing.conditioner_ml_per_litre;
            let rationale = format!(
                "{:.0}% surface algae coverage detected — dose conditioner now rather \
                 than waiting for the next top-off",
                algae.coverage * 100.0
            );
            tasks = vec![
                Task::new(
                    TaskKind::AddConditioner,
                    Target::Garden,
                    severity,
                    DueWindow::within_days(state.now, 2.0),
                    rationale,
                    Self::ID,
                )
                .with_detail(TaskDetail::Dose { millilitres: ml }),
            ];
        }

        tasks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use garden_core::{
        AlgaeReading, Planting, PlantingId, SlotId, Timestamp, VarietyId, time::add_days,
    };

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    /// A garden with one planting germinated `days_ago`.
    fn garden_with(variety: &str, germinated_days_ago: f64) -> GardenState {
        let mut g = GardenState::new_studio_2(t0());
        let mut p = Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new(variety),
            add_days(t0(), -(germinated_days_ago + 6.0)),
        );
        p.germinated_at = Some(add_days(t0(), -germinated_days_ago));
        g.plantings.push(p);
        g
    }

    fn food_engine() -> Engine {
        Engine::new(vec![
            Box::new(PlantFoodByVolumeRule),
            Box::new(PlantFoodByEcRule),
        ])
    }

    #[test]
    fn no_food_while_seeds_are_still_germinating() {
        let mut g = GardenState::new_studio_2(t0());
        g.plantings.push(Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new("kale-lacinato"),
            t0(),
        ));
        g.tank.litres_added_since_food_dose = 6.0;
        assert!(food_engine().evaluate(&g).tasks.is_empty());
    }

    #[test]
    fn seedlings_get_a_reduced_dose() {
        let mut g = garden_with("kale-lacinato", 5.0); // seedling
        g.tank.litres_added_since_food_dose = 4.0;
        let tasks = food_engine().evaluate(&g).tasks;
        match tasks[0].detail {
            // 4 L * 2 mL/L * 0.5 sprout fraction
            Some(TaskDetail::Dose { millilitres }) => assert!((millilitres - 4.0).abs() < 0.01),
            other => panic!("expected a dose, got {other:?}"),
        }
        assert!(tasks[0].rationale.contains("sprout strength"));
    }

    #[test]
    fn established_plants_get_full_strength() {
        let mut g = garden_with("kale-lacinato", 40.0); // mature
        g.tank.litres_added_since_food_dose = 4.0;
        let tasks = food_engine().evaluate(&g).tasks;
        match tasks[0].detail {
            Some(TaskDetail::Dose { millilitres }) => assert!((millilitres - 8.0).abs() < 0.01),
            other => panic!("expected a dose, got {other:?}"),
        }
    }

    #[test]
    fn a_small_top_off_does_not_warrant_feeding() {
        let mut g = garden_with("kale-lacinato", 40.0);
        g.tank.litres_added_since_food_dose = 0.5;
        assert!(food_engine().evaluate(&g).tasks.is_empty());
    }

    #[test]
    fn the_estimate_rule_admits_it_is_an_estimate() {
        let mut g = garden_with("kale-lacinato", 40.0);
        g.tank.litres_added_since_food_dose = 3.0;
        let tasks = food_engine().evaluate(&g).tasks;
        assert!(tasks[0].rationale.contains("volume estimate"));
        assert_eq!(tasks[0].source, PlantFoodByVolumeRule::ID);
    }

    #[test]
    fn fitting_an_ec_probe_switches_to_measurement() {
        let mut g = garden_with("kale-lacinato", 40.0);
        g.tank.litres_added_since_food_dose = 3.0;
        g.capabilities.insert(Capability::Conductivity);
        g.sensors.ec_ms_cm = Some(0.4); // well under the 0.8-1.4 leafy band

        let eval = food_engine().evaluate(&g);
        assert_eq!(eval.tasks.len(), 1);
        assert_eq!(eval.tasks[0].source, PlantFoodByEcRule::ID);
        assert!(eval.tasks[0].rationale.contains("EC 0.40"));
        assert!(eval.was_suppressed("plant-food-by-volume"));
    }

    #[test]
    fn on_target_ec_means_no_task_even_after_topping_off() {
        let mut g = garden_with("kale-lacinato", 40.0);
        g.tank.litres_added_since_food_dose = 6.0; // the estimate rule would fire
        g.capabilities.insert(Capability::Conductivity);
        g.sensors.ec_ms_cm = Some(1.1); // but the solution is actually fine

        // This is the whole point of the probe: it prevents over-feeding.
        assert!(food_engine().evaluate(&g).tasks.is_empty());
    }

    #[test]
    fn severely_depleted_solution_escalates() {
        let mut g = garden_with("kale-lacinato", 40.0);
        g.capabilities.insert(Capability::Conductivity);
        g.sensors.ec_ms_cm = Some(0.2);
        assert_eq!(food_engine().evaluate(&g).tasks[0].severity, Severity::Urgent);
    }

    #[test]
    fn the_measured_rule_still_covers_the_volume_case_if_the_reading_drops_out() {
        // Capability present but the reading is momentarily missing. Because the EC
        // rule owns the kind outright, it must not leave a hole.
        let mut g = garden_with("kale-lacinato", 40.0);
        g.tank.litres_added_since_food_dose = 3.0;
        g.capabilities.insert(Capability::Conductivity);
        g.sensors.ec_ms_cm = None;

        let tasks = food_engine().evaluate(&g).tasks;
        assert_eq!(tasks.len(), 1, "must not go silent");
        assert!(tasks[0].rationale.contains("volume estimate"));
    }

    #[test]
    fn mixed_leafy_and_fruiting_tanks_are_flagged_as_a_compromise() {
        let mut g = garden_with("kale-lacinato", 40.0);
        let mut tomato = Planting::new(
            PlantingId(2),
            SlotId(1),
            VarietyId::new("red-cherry-tomato"),
            add_days(t0(), -80.0),
        );
        tomato.germinated_at = Some(add_days(t0(), -70.0));
        g.plantings.push(tomato);
        g.capabilities.insert(Capability::Conductivity);
        g.sensors.ec_ms_cm = Some(0.5);

        let tasks = food_engine().evaluate(&g).tasks;
        assert!(tasks[0].rationale.contains("compromise"), "{}", tasks[0].rationale);
    }

    #[test]
    fn conditioner_is_owed_after_an_untreated_top_off() {
        let mut g = garden_with("kale-lacinato", 40.0);
        g.tank.last_conditioner = Some(add_days(t0(), -3.0));
        g.tank.last_top_off = Some(add_days(t0(), -1.0));
        let tasks = Engine::new(vec![Box::new(ConditionerRule)]).evaluate(&g).tasks;
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].rationale.contains("without conditioner"));
    }

    #[test]
    fn conditioner_is_quiet_when_it_went_in_with_the_water() {
        let mut g = garden_with("kale-lacinato", 40.0);
        g.tank.last_top_off = Some(add_days(t0(), -2.0));
        g.tank.last_conditioner = Some(add_days(t0(), -2.0));
        assert!(Engine::new(vec![Box::new(ConditionerRule)]).evaluate(&g).tasks.is_empty());
    }

    #[test]
    fn algae_pulls_the_conditioner_dose_forward() {
        let mut g = garden_with("kale-lacinato", 40.0);
        g.tank.last_top_off = Some(add_days(t0(), -1.0));
        g.tank.last_conditioner = Some(add_days(t0(), -1.0)); // cadence rule would be quiet
        g.capabilities.insert(Capability::CanopyMetrics);
        g.algae = Some(AlgaeReading {
            at: t0(),
            coverage: 0.30,
        });

        let eval = Engine::new(vec![
            Box::new(ConditionerRule),
            Box::new(ConditionerByAlgaeRule),
        ])
        .evaluate(&g);

        assert_eq!(eval.tasks.len(), 1);
        assert_eq!(eval.tasks[0].severity, Severity::Important);
        assert!(eval.tasks[0].rationale.contains("algae"));
    }
}
