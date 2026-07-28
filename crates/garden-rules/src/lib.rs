//! The rule set: what the garden should be told to do, and why.
//!
//! See [`engine`] for how capability-based precedence works. The short version: every
//! rule declares the sensors or vision stages it needs, and a rule backed by a real
//! measurement displaces the calendar estimate for the same task kind the moment its
//! hardware appears.

pub mod engine;
pub mod harvest;
pub mod maintenance;
pub mod nutrients;
pub mod plants;
pub mod roots;
pub mod rootzone;
pub mod succession;
pub mod water;

pub use succession::{Suggestion, suggest};

pub use engine::{
    Engine, Evaluation, PRECEDENCE_FALLBACK, PRECEDENCE_MEASURED, Rule, Suppression,
    SuppressionReason,
};

use harvest::{HarvestByCalendarRule, HarvestByCanopyRule, ReplantRule};
use maintenance::{
    DeepCleanByCalendarRule, DeepCleanByFoulingRule, TankRefreshByChlorosisRule, TankRefreshRule,
};
use nutrients::{
    ConditionerByAlgaeRule, ConditionerRule, PlantFoodByEcRule, PlantFoodByVolumeRule,
};
use plants::{
    GerminationCheckRule, PollinationRule, PrunePlantByCanopyRule, PrunePlantRule,
    ThinByCalendarRule, ThinBySegmentationRule,
};
use roots::{RootPruneByFlowRule, RootPruneCadenceRule};
use rootzone::{PhRule, RootZoneTempRule};
use water::WaterLevelRule;

/// Every rule, registered together.
///
/// Rules whose hardware is absent are filtered out at evaluation time rather than
/// here, so this list stays the same whether or not the EC probe has been bought and
/// whether or not vision is switched on.
pub fn default_rules() -> Vec<Box<dyn Rule>> {
    vec![
        // Water
        Box::new(WaterLevelRule),
        // Nutrients
        Box::new(PlantFoodByVolumeRule),
        Box::new(PlantFoodByEcRule),
        Box::new(ConditionerRule),
        Box::new(ConditionerByAlgaeRule),
        // Roots
        Box::new(RootPruneCadenceRule),
        Box::new(RootPruneByFlowRule),
        // Harvest and turnover
        Box::new(HarvestByCalendarRule),
        Box::new(HarvestByCanopyRule),
        Box::new(ReplantRule),
        // Per-plant work
        Box::new(GerminationCheckRule),
        Box::new(ThinByCalendarRule),
        Box::new(ThinBySegmentationRule),
        Box::new(PrunePlantRule),
        Box::new(PrunePlantByCanopyRule),
        Box::new(PollinationRule),
        // Maintenance
        Box::new(TankRefreshRule),
        Box::new(TankRefreshByChlorosisRule),
        Box::new(DeepCleanByCalendarRule),
        Box::new(DeepCleanByFoulingRule),
        // Root-zone chemistry
        Box::new(RootZoneTempRule),
        Box::new(PhRule),
    ]
}

/// An engine with the full rule set.
pub fn default_engine() -> Engine {
    Engine::new(default_rules())
}

#[cfg(test)]
mod tests {
    use super::*;
    use garden_core::{
        Capability, CapabilitySet, GardenState, Planting, PlantingId, SlotId, Timestamp, VarietyId,
        time::add_days,
    };

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    /// A neglected garden: nearly dry, unfed, unpruned, never cleaned.
    fn neglected() -> GardenState {
        let mut g = GardenState::new_studio_2(t0());
        for (i, variety) in ["kale-lacinato", "basil", "red-cherry-tomato"]
            .iter()
            .enumerate()
        {
            let mut p = Planting::new(
                PlantingId(i as u64),
                SlotId(i as u8),
                VarietyId::new(*variety),
                add_days(t0(), -100.0),
            );
            p.germinated_at = Some(add_days(t0(), -92.0));
            g.plantings.push(p);
        }
        g.tank.volume_l = 2.0;
        g.tank.consumption_lpd = 1.2;
        g.tank.litres_added_since_food_dose = 8.0;
        g.tank.last_top_off = Some(add_days(t0(), -1.0));
        g.sensors.water_level_mm = Some(300.0);
        g
    }

    #[test]
    fn every_rule_is_registered_exactly_once() {
        let rules = default_rules();
        let mut ids: Vec<_> = rules.iter().map(|r| r.id()).collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate rule id registered");
        assert_eq!(before, 22);
    }

    #[test]
    fn a_stock_garden_produces_actionable_work() {
        let eval = default_engine().evaluate(&neglected());
        assert!(!eval.tasks.is_empty());
        // Most severe first, and a nearly dry tank should lead.
        assert_eq!(eval.tasks[0].kind, garden_core::TaskKind::AddWater);
    }

    #[test]
    fn every_task_carries_a_rationale_and_a_source() {
        let eval = default_engine().evaluate(&neglected());
        for task in &eval.tasks {
            assert!(
                !task.rationale.is_empty(),
                "{:?} has no rationale",
                task.kind
            );
            assert!(!task.source.as_str().is_empty());
        }
    }

    #[test]
    fn task_keys_are_unique_within_one_evaluation() {
        let eval = default_engine().evaluate(&neglected());
        let mut keys: Vec<_> = eval.tasks.iter().map(|t| t.key.clone()).collect();
        let before = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(
            keys.len(),
            before,
            "duplicate task keys reached the operator"
        );
    }

    #[test]
    fn deferred_hardware_rules_report_why_they_are_inactive() {
        let eval = default_engine().evaluate(&neglected());
        assert!(eval.was_suppressed("plant-food-by-ec"));
        assert!(eval.was_suppressed("solution-ph"));
        assert!(eval.was_suppressed("harvest-by-canopy"));
        assert!(eval.was_suppressed("thin-by-segmentation"));

        let ec = eval
            .suppressed
            .iter()
            .find(|s| s.rule.as_str() == "plant-food-by-ec")
            .unwrap();
        assert_eq!(ec.explain(), "plant-food-by-ec needs EC probe");
    }

    #[test]
    fn fully_equipping_the_garden_leaves_no_hardware_gaps() {
        let mut g = neglected();
        g.capabilities = CapabilitySet::fully_equipped();
        let eval = default_engine().evaluate(&g);

        let hardware_gaps: Vec<_> = eval
            .suppressed
            .iter()
            .filter(|s| matches!(s.reason, SuppressionReason::MissingCapabilities(_)))
            .collect();
        assert!(
            hardware_gaps.is_empty(),
            "unexpected gaps: {hardware_gaps:?}"
        );
    }

    #[test]
    fn adding_a_capability_never_leaves_a_task_kind_uncovered() {
        // The core risk of precedence-based replacement: a measured rule wins a kind,
        // then declines to handle the case its fallback used to, and work silently
        // disappears. Walk the capability ladder and assert nothing is dropped.
        //
        // No sensor *readings* are supplied here, only the capability, which is the
        // adversarial case: the measured rule must still fall through to cadence.
        let base = neglected();
        let kinds_before: Vec<_> = default_engine()
            .evaluate(&base)
            .tasks
            .iter()
            .map(|t| t.kind)
            .collect();

        for capability in [
            Capability::Conductivity,
            Capability::CanopyMetrics,
            Capability::PlantSegmentation,
            Capability::WaterTemperature,
        ] {
            let mut upgraded = base.clone();
            upgraded.capabilities.insert(capability);
            let after = default_engine().evaluate(&upgraded);

            for kind in &kinds_before {
                assert!(
                    after.tasks.iter().any(|t| t.kind == *kind),
                    "enabling {capability} dropped all '{kind}' tasks"
                );
            }
        }
    }

    #[test]
    fn an_empty_garden_generates_no_busywork() {
        let g = GardenState::new_studio_2(t0());
        let eval = default_engine().evaluate(&g);
        assert!(eval.tasks.is_empty(), "unexpected: {:?}", eval.tasks);
    }
}

#[cfg(test)]
mod advisory_only {
    //! The guarantee that a language model cannot dose the tank.
    //!
    //! `SlotMetrics::diagnosis` is free text written by a vision model. Deterministic
    //! rules own everything that touches water, nutrients or an actuator, and the way
    //! that is enforced is simply that no rule reads the field. Which is easy to
    //! believe today and easy to break in six months, so it is a test.

    use garden_core::{GardenState, SlotMetrics, Timestamp};

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    #[test]
    fn no_rule_changes_its_mind_because_of_a_diagnosis() {
        // The same garden evaluated twice: once with every slot undiagnosed, once with
        // an alarming sentence attached to each. If any task differs, something is
        // reading it.
        let mut plain = GardenState::new_studio_2(t0());
        for slot in plain.geometry.slots() {
            let mut metrics = SlotMetrics::new(slot, t0(), 250.0);
            metrics.green_fraction = 0.7;
            metrics.plant_count = Some(2);
            plain.slot_metrics.insert(slot, metrics);
        }
        plain
            .capabilities
            .insert(garden_core::Capability::CanopyMetrics);

        let mut diagnosed = plain.clone();
        for metrics in diagnosed.slot_metrics.values_mut() {
            metrics.diagnosis = Some(
                "Severe nitrogen deficiency and spider mites. Dose immediately and \
                 refresh the tank."
                    .into(),
            );
        }
        diagnosed
            .capabilities
            .insert(garden_core::Capability::VisualDiagnosis);

        let engine = super::default_engine();
        let before = engine.evaluate(&plain);
        let after = engine.evaluate(&diagnosed);

        let keys = |e: &super::Evaluation| -> Vec<String> {
            let mut k: Vec<String> = e.tasks.iter().map(|t| t.key.0.clone()).collect();
            k.sort();
            k
        };
        assert_eq!(
            keys(&before),
            keys(&after),
            "a rule is reading SlotMetrics::diagnosis — it must not, because that text \
             comes from a language model and these tasks dose the tank"
        );

        // ...and the wording is untouched too, so nothing is quoting it into advice.
        for (a, b) in before.tasks.iter().zip(after.tasks.iter()) {
            assert_eq!(a.rationale, b.rationale, "a rationale quotes the diagnosis");
            assert_eq!(a.severity, b.severity, "a severity moved on model output");
        }
    }

    #[test]
    fn the_field_is_not_mentioned_anywhere_in_this_crate() {
        // Belt to the braces above: the behavioural test only covers states it happens
        // to construct. This covers the source.
        let sources = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
            .expect("the crate's own source");
        for entry in sources.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            // This file names it, in these tests, which is the point.
            if path.file_name().is_some_and(|n| n == "lib.rs") {
                continue;
            }
            assert!(
                !text.contains("diagnosis"),
                "{} mentions `diagnosis`; rules must not read model output",
                path.display()
            );
        }
    }
}
