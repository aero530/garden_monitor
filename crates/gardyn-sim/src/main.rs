//! Runs a season against the rule engine and prints what happened.
//!
//! Usage: `cargo run -p gardyn-sim [days]`

use gardyn_core::{Capability, GardenState, SlotId, Timestamp};
use gardyn_rules::{Engine, SuppressionReason, default_rules};
use gardyn_sim::Simulation;
use gardyn_sim::scenario::{Operator, Report, run};

/// A plausible Studio 2 planting: mostly greens and herbs, one fruiting plant.
const PLANTING: &[(u8, &str)] = &[
    (0, "kale-lacinato"),
    (1, "butterhead"),
    (2, "basil"),
    (3, "red-swiss-chard"),
    (8, "arugula"),
    (9, "cilantro"),
    (10, "green-bok-choy"),
    (11, "red-cherry-tomato"),
];

fn stocked(seed: u64, capabilities: &[Capability]) -> Simulation {
    // A fixed start instant keeps runs reproducible; wall-clock time would not.
    let start = Timestamp::from_second(1_700_000_000).unwrap();
    let mut sim = Simulation::new(seed, start);
    for (slot, variety) in PLANTING {
        sim.plant(SlotId(*slot), variety);
    }
    for capability in capabilities {
        sim.enable(*capability);
    }
    sim
}

fn main() {
    let days: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(120);
    let seed = 2026;

    println!("Gardyn Studio 2 — {days}-day simulation, seed {seed}\n");

    print_capability_report(&stocked(seed, &[]).state, "Stock hardware");
    let equipped = [
        Capability::WaterTemperature,
        Capability::CanopyMetrics,
        Capability::Conductivity,
        Capability::PlantSegmentation,
    ];
    print_capability_report(&stocked(seed, &equipped).state, "Fully equipped");

    println!("\n=== Operator comparison (stock hardware) ===\n");
    let operators = [Operator::DILIGENT, Operator::TYPICAL, Operator::BUSY];
    let stock_reports: Vec<Report> = operators
        .iter()
        .map(|op| run(&mut stocked(seed, &[]), *op, days, seed))
        .collect();
    print_table(&stock_reports);

    println!("\n=== Same operator, hardware added incrementally ===\n");
    let ladder: [(&str, &[Capability]); 4] = [
        ("stock", &[]),
        ("+ water temp", &[Capability::WaterTemperature]),
        (
            "+ canopy vision",
            &[Capability::WaterTemperature, Capability::CanopyMetrics],
        ),
        (
            "+ EC probe",
            &[
                Capability::WaterTemperature,
                Capability::CanopyMetrics,
                Capability::Conductivity,
            ],
        ),
    ];

    println!(
        "{:<18} {:>9} {:>9} {:>10} {:>9} {:>8}",
        "configuration", "harvest", "canopy", "interrupts", "dry days", "tasks"
    );
    println!("{}", "-".repeat(68));
    for (label, capabilities) in ladder {
        let mut sim = stocked(seed, capabilities);
        let r = run(&mut sim, Operator::TYPICAL, days, seed);
        println!(
            "{:<18} {:>9.0} {:>9.0} {:>10.1} {:>9} {:>8}",
            label,
            r.harvested_cm2,
            r.final_canopy_cm2,
            r.interruptions_per_week(),
            r.dry_days,
            r.total_completed()
        );
    }

    println!("\nharvest and canopy in cm²; interrupts per week; a 'typical' operator");
    println!("acts on ~55% of what they are shown and ignores anything below advisory.");
}

fn print_table(reports: &[Report]) {
    println!(
        "{:<10} {:>9} {:>9} {:>10} {:>9} {:>8} {:>8}",
        "operator", "harvest", "canopy", "interrupts", "dry days", "done", "ignored"
    );
    println!("{}", "-".repeat(70));
    for r in reports {
        println!(
            "{:<10} {:>9.0} {:>9.0} {:>10.1} {:>9} {:>8} {:>8}",
            r.operator,
            r.harvested_cm2,
            r.final_canopy_cm2,
            r.interruptions_per_week(),
            r.dry_days,
            r.total_completed(),
            r.ignored
        );
    }
}

/// Show which rules are live for a given hardware configuration, and why the rest
/// are not. This is the operator-facing answer to "what is my system actually doing?"
fn print_capability_report(state: &GardenState, label: &str) {
    let engine = Engine::new(default_rules());
    let evaluation = engine.evaluate(state);

    let mut gaps = Vec::new();
    let mut outranked = 0;
    for s in &evaluation.suppressed {
        match &s.reason {
            SuppressionReason::MissingCapabilities(caps) => gaps.push((s.rule.clone(), caps.clone())),
            SuppressionReason::Outranked { .. } => outranked += 1,
        }
    }

    println!(
        "{label}: {} of {} rules active ({outranked} superseded by better-informed \
         rules, {} missing hardware)",
        evaluation.active.len(),
        engine.rule_count(),
        gaps.len()
    );
    for (rule, caps) in &gaps {
        let names: Vec<_> = caps.iter().map(|c| c.label()).collect();
        println!("    waiting on hardware: {rule} (needs {})", names.join(", "));
    }
    if gaps.is_empty() {
        println!("    every rule has the hardware it needs");
    }
}
