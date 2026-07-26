//! Telemetry for gardens that do not have any yet.
//!
//! The edge agent does not exist until Phase 1, so a real device has nothing to show.
//! A garden registered as [`DeviceModel::Simulated`] is driven by `gardyn-sim`
//! instead, which makes the whole UI — dashboards, rules, task lifecycle, sharing —
//! exercisable end to end today, and gives someone a way to try the system before
//! taking a screwdriver to their Studio.
//!
//! Real gardens deliberately show an empty state rather than invented numbers.

use gardyn_core::{DeviceModel, Garden, GardenState, SlotId};
use gardyn_sim::Simulation;
use gardyn_sim::scenario::Operator;
use jiff::Timestamp;

/// What a simulated garden is planted with.
const PLANTING: &[(u8, &str)] = &[
    (0, "kale-lacinato"),
    (1, "lettuce-butterhead"),
    (2, "basil-genovese"),
    (3, "swiss-chard"),
    (8, "arugula"),
    (9, "cilantro"),
    (10, "bok-choy"),
    (11, "tomato-cherry"),
];

/// Bound the work a page load can do. Six months of simulated time is a few
/// milliseconds, but the cap keeps an old garden from turning into a slow page.
const MAX_SIMULATED_DAYS: f64 = 180.0;

/// Current state for a garden, or `None` when there is genuinely nothing to report.
pub fn state_for(garden: &Garden, now: Timestamp) -> Option<GardenState> {
    if garden.model != DeviceModel::Simulated {
        return None;
    }

    // Seeded from the garden id so a given garden always tells the same story, and
    // two simulated gardens on one account look different from each other.
    let seed = (garden.id.as_uuid().as_u128() as u64) | 1;
    let mut sim = Simulation::new(seed, garden.created_at);
    sim.state.garden = garden.id;

    for (slot, variety) in PLANTING {
        sim.plant(SlotId(*slot), variety);
    }
    for capability in [
        gardyn_core::Capability::WaterTemperature,
        gardyn_core::Capability::CanopyMetrics,
    ] {
        sim.enable(capability);
    }

    let elapsed = gardyn_core::time::days_between(garden.created_at, now);
    let days = elapsed.clamp(1.0, MAX_SIMULATED_DAYS) as u32;

    // A "typical" operator, so the garden looks lived-in: mostly tended, with some
    // outstanding work. A perfectly maintained garden would make an empty dashboard.
    gardyn_sim::scenario::run(&mut sim, Operator::TYPICAL, days, seed);

    sim.state.garden = garden.id;
    sim.state.now = now;
    Some(sim.state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gardyn_core::time::add_days;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn garden(model: DeviceModel) -> Garden {
        Garden::new("Kitchen", model, t0())
    }

    #[test]
    fn a_real_garden_reports_nothing_rather_than_inventing_data() {
        assert!(state_for(&garden(DeviceModel::Studio2), add_days(t0(), 30.0)).is_none());
    }

    #[test]
    fn a_simulated_garden_produces_a_populated_state() {
        let g = garden(DeviceModel::Simulated);
        let state = state_for(&g, add_days(t0(), 60.0)).unwrap();
        assert_eq!(state.garden, g.id);
        assert_eq!(state.occupied_slots(), PLANTING.len());
        assert!(state.sensors.water_level_mm.is_some());
    }

    #[test]
    fn the_state_is_attributed_to_the_right_garden() {
        // A mix-up here would render one garden's data under another's name.
        let a = garden(DeviceModel::Simulated);
        let b = garden(DeviceModel::Simulated);
        assert_eq!(state_for(&a, add_days(t0(), 10.0)).unwrap().garden, a.id);
        assert_eq!(state_for(&b, add_days(t0(), 10.0)).unwrap().garden, b.id);
    }

    #[test]
    fn the_same_garden_tells_the_same_story_on_every_page_load() {
        let g = garden(DeviceModel::Simulated);
        let at = add_days(t0(), 45.0);
        let first = state_for(&g, at).unwrap();
        let second = state_for(&g, at).unwrap();
        assert_eq!(first.tank.volume_l, second.tank.volume_l);
    }

    #[test]
    fn two_gardens_do_not_look_identical() {
        let at = add_days(t0(), 45.0);
        let a = state_for(&garden(DeviceModel::Simulated), at).unwrap();
        let b = state_for(&garden(DeviceModel::Simulated), at).unwrap();
        assert_ne!(a.tank.volume_l, b.tank.volume_l);
    }

    #[test]
    fn a_brand_new_garden_does_not_panic_on_zero_elapsed_days() {
        assert!(state_for(&garden(DeviceModel::Simulated), t0()).is_some());
    }
}
