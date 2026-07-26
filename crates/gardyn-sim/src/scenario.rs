//! Running a season and measuring what the rule set actually achieves.
//!
//! A rule set is only as good as the behaviour it produces over months, against an
//! operator who does not do everything they are told. This module closes that loop:
//! simulate, evaluate, let a modelled operator respond, and report the outcome.

use crate::Simulation;
use crate::physics::Lcg;
use gardyn_core::{Severity, TaskKind};
use gardyn_rules::Engine;
use std::collections::BTreeMap;

/// How a person actually responds to notifications.
#[derive(Debug, Clone, Copy)]
pub struct Operator {
    pub name: &'static str,
    /// Notifications quieter than this are ignored outright.
    pub attends_from: Severity,
    /// Probability of acting on a task they do attend to, per day.
    pub reliability: f32,
}

impl Operator {
    /// Does everything, promptly. The upper bound on what the rules can achieve.
    pub const DILIGENT: Operator = Operator {
        name: "diligent",
        attends_from: Severity::Info,
        reliability: 1.0,
    };

    /// Reads pushes, skims the daily brief, forgets things. The realistic case, and
    /// the one worth tuning against.
    pub const TYPICAL: Operator = Operator {
        name: "typical",
        attends_from: Severity::Advisory,
        reliability: 0.55,
    };

    /// Only reacts when something is shouting. The stress test.
    pub const BUSY: Operator = Operator {
        name: "busy",
        attends_from: Severity::Urgent,
        reliability: 0.8,
    };

    fn will_act(&self, severity: Severity, rng: &mut Lcg) -> bool {
        severity >= self.attends_from && rng.chance(self.reliability)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub operator: &'static str,
    pub days: u32,
    pub completed: BTreeMap<TaskKind, u32>,
    pub raised: BTreeMap<Severity, u32>,
    pub ignored: u32,
    pub harvested_cm2: f32,
    pub final_canopy_cm2: f32,
    pub dry_days: u32,
    pub peak_restriction: f32,
    /// Notifications that would actually have interrupted the operator.
    pub interruptions: u32,
}

impl Report {
    pub fn total_completed(&self) -> u32 {
        self.completed.values().sum()
    }

    /// Interruptions per week. The number that decides whether the system gets muted.
    pub fn interruptions_per_week(&self) -> f32 {
        if self.days == 0 {
            return 0.0;
        }
        self.interruptions as f32 / (self.days as f32 / 7.0)
    }
}

/// Run `days` of simulated time, evaluating rules once per day.
pub fn run(sim: &mut Simulation, operator: Operator, days: u32, seed: u64) -> Report {
    let engine = Engine::new(gardyn_rules::default_rules());
    let mut rng = Lcg::new(seed ^ 0xA5A5_5A5A);
    let mut report = Report {
        operator: operator.name,
        days,
        ..Default::default()
    };

    // Severity at which each outstanding task was last announced.
    //
    // Rules re-emit every tick by design — they describe what should be true now, not
    // what has changed. Counting each emission as a notification would report a task
    // the operator is ignoring as dozens of alerts a week. The brain dedupes by key
    // and only re-announces on escalation, so the simulation must too, or the noise
    // metric measures nothing.
    let mut announced: BTreeMap<gardyn_core::TaskKey, Severity> = BTreeMap::new();

    for _ in 0..days {
        sim.tick(1.0);

        let evaluation = engine.evaluate(&sim.state);

        for task in &evaluation.tasks {
            let previous = announced.get(&task.key).copied();
            let worth_announcing = match previous {
                None => true,
                Some(before) => task.severity > before,
            };

            if worth_announcing {
                *report.raised.entry(task.severity).or_default() += 1;
                if task.severity.interrupts() {
                    report.interruptions += 1;
                }
            }
            announced.insert(task.key.clone(), task.severity);

            if operator.will_act(task.severity, &mut rng) {
                sim.perform(task);
                *report.completed.entry(task.kind).or_default() += 1;
                announced.remove(&task.key);
            } else if worth_announcing {
                report.ignored += 1;
            }
        }

        // A task the rules stopped emitting has resolved itself; forget it so that a
        // recurrence is announced afresh.
        announced.retain(|key, _| evaluation.tasks.iter().any(|t| &t.key == key));

        report.peak_restriction = report
            .peak_restriction
            .max(sim.state.pump.restriction_ratio());
    }

    report.harvested_cm2 = sim.harvested_cm2;
    report.final_canopy_cm2 = sim.standing_canopy_cm2();
    report.dry_days = sim.dry_days;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use gardyn_core::{Capability, SlotId, Timestamp};

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn stocked(seed: u64) -> Simulation {
        let mut sim = Simulation::new(seed, t0());
        sim.plant(SlotId(0), "kale-lacinato");
        sim.plant(SlotId(1), "butterhead");
        sim.plant(SlotId(2), "basil");
        sim.plant(SlotId(8), "arugula");
        sim
    }

    #[test]
    fn a_diligent_operator_keeps_the_garden_alive() {
        let mut sim = stocked(11);
        let report = run(&mut sim, Operator::DILIGENT, 120, 11);

        assert_eq!(report.dry_days, 0, "the tank should never have run dry");
        assert!(report.harvested_cm2 > 0.0, "should have harvested something");
        assert!(report.total_completed() > 20);
    }

    /// Grow a garden to maturity under ideal care, then hand it to a scenario.
    fn established(seed: u64, days: u32) -> Simulation {
        let mut sim = stocked(seed);
        for _ in 0..days {
            sim.tick(1.0);
            sim.state.tank.volume_l = sim.state.tank_geometry.capacity_l;
            sim.feed_to_full_strength();
        }
        sim
    }

    #[test]
    fn neglect_costs_yield() {
        let good = run(&mut stocked(11), Operator::DILIGENT, 120, 11);
        let bad = run(&mut stocked(11), Operator::BUSY, 120, 11);

        assert!(
            bad.harvested_cm2 < good.harvested_cm2,
            "neglect should cost yield: {:.0} vs {:.0}",
            bad.harvested_cm2,
            good.harvested_cm2
        );
        assert!(
            bad.final_canopy_cm2 < good.final_canopy_cm2,
            "neglect should cost growth"
        );
    }

    #[test]
    fn a_starved_garden_does_not_run_dry() {
        // Counter-intuitive but correct, and worth pinning down: an operator who
        // ignores feeding gets stunted plants, and stunted plants barely transpire.
        // Drought is a symptom of a *thriving* garden that is not kept topped up, so
        // the water rule must not be the only thing standing between neglect and loss.
        let report = run(&mut stocked(11), Operator::BUSY, 120, 11);
        assert_eq!(report.dry_days, 0);
        assert!(
            report.final_canopy_cm2 < 400.0,
            "expected stunted growth, got {:.0} cm²",
            report.final_canopy_cm2
        );
    }

    #[test]
    fn a_thriving_garden_escalates_loudly_enough_to_reach_a_busy_operator() {
        // An established garden drinks fast. If it is not topped up, the water rule
        // has to reach someone who ignores everything below Urgent.
        let mut sim = established(5, 45);
        let report = run(&mut sim, Operator::BUSY, 60, 5);
        assert!(
            report.completed.contains_key(&TaskKind::AddWater),
            "even a busy operator must end up watering: {:?}",
            report.completed
        );
    }

    #[test]
    fn interruption_rate_stays_tolerable_for_an_attentive_operator() {
        // If the system interrupts constantly it gets muted, and then it is worthless.
        let mut sim = stocked(7);
        let report = run(&mut sim, Operator::DILIGENT, 120, 7);
        assert!(
            report.interruptions_per_week() < 7.0,
            "too noisy: {:.1}/week",
            report.interruptions_per_week()
        );
    }

    #[test]
    fn adding_sensors_does_not_make_the_system_noisier() {
        // A real risk: every capability added lights up more rules, and the operator
        // drowns. Measure it rather than hoping.
        let mut stock = stocked(3);
        let stock_report = run(&mut stock, Operator::DILIGENT, 120, 3);

        let mut equipped = stocked(3);
        equipped
            .enable(Capability::WaterTemperature)
            .enable(Capability::Conductivity)
            .enable(Capability::CanopyMetrics)
            .enable(Capability::PlantSegmentation);
        let equipped_report = run(&mut equipped, Operator::DILIGENT, 120, 3);

        assert!(
            equipped_report.interruptions_per_week()
                <= stock_report.interruptions_per_week() * 1.5,
            "equipping the garden should not flood the operator: {:.1}/week stock \
             vs {:.1}/week equipped",
            stock_report.interruptions_per_week(),
            equipped_report.interruptions_per_week()
        );
    }

    #[test]
    fn scenarios_are_reproducible() {
        let a = run(&mut stocked(21), Operator::TYPICAL, 90, 21);
        let b = run(&mut stocked(21), Operator::TYPICAL, 90, 21);
        assert_eq!(a.total_completed(), b.total_completed());
        assert_eq!(a.harvested_cm2, b.harvested_cm2);
        assert_eq!(a.dry_days, b.dry_days);
    }
}
