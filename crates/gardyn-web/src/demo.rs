//! Simulated telemetry, for gardens with no hardware.
//!
//! The edge agent does not exist until Phase 1, so a garden registered as
//! [`DeviceModel::Simulated`] gets sensor readings, canopy metrics, and camera frames
//! from the physics model instead. That makes the whole application — dashboards,
//! rules, task lifecycle, camera, sharing — exercisable today, and gives someone a way
//! to try the system before taking a screwdriver to their Studio.
//!
//! Plantings are **not** simulated. They come from the database like any other
//! garden's, so the slot UI behaves identically whether or not the hardware is real.
//! The simulator supplies the physics; the operator supplies the plants.

use gardyn_core::{Capability, DeviceModel, Garden, GardenState, SlotId, Timestamp, VarietyId};
use gardyn_sim::Simulation;
use gardyn_sim::scenario::Operator;

/// Gardyn's "Salad Lover" welcome kit, exactly as their placement card lays it out.
///
/// Sixteen plants for a Studio 2, in the published order: left column top to bottom,
/// then right column. The card's colour coding is the *slot's* light intensity, not
/// the plant's requirement — every plant here sits in a slot at or above what its own
/// article asks for, which is what makes the arrangement worth copying verbatim rather
/// than re-deriving.
///
/// Written into the `plantings` table as ordinary rows, so they can be harvested,
/// replaced or pulled like anything else.
pub const SALAD_LOVER_KIT: &[(u8, &str)] = &[
    // Left column, top to bottom.
    (0, "watercress"),         // medium slot
    (1, "red-mustard"),        // medium
    (2, "romaine"),            // high
    (3, "breen"),              // medium
    (4, "basil"),              // high
    (5, "green-salanova"),     // medium
    (6, "green-tatsoi"),       // high
    (7, "bunching-onions"),    // low
    // Right column, top to bottom.
    (8, "cilantro"),           // low
    (9, "white-kohlrabi"),     // medium — the card says just "Kohlrabi"
    (10, "butterhead"),        // medium
    (11, "sunflower"),         // high
    (12, "perpetual-spinach"), // medium
    (13, "bronze-arrow"),      // high
    (14, "kale"),              // medium — the card's "Classic Kale"
    (15, "bulls-blood-beets"), // medium — the card's "Bull's Blood"
];

/// Bound the work a page load can do. Six months of simulated time is a few
/// milliseconds, but the cap keeps an old garden from turning into a slow page.
const MAX_SIMULATED_DAYS: f64 = 180.0;

/// How often a simulated garden takes a picture of itself.
const FRAME_INTERVAL_MINUTES: f64 = 30.0;

/// Seed the plantings a new simulated garden starts with.
///
/// Germination is stamped too. Without it every seeded cube would sit at
/// [`Stage::Seeded`](gardyn_core::Stage::Seeded) — the gate on thinning, harvest,
/// root checks and replanting — and a garden created to demonstrate the system would
/// demonstrate an empty task list.
pub async fn seed_plantings(
    store: &gardyn_store::Store,
    garden: &Garden,
    by: Option<gardyn_auth::UserId>,
) -> gardyn_store::Result<()> {
    if garden.model != DeviceModel::Simulated {
        return Ok(());
    }

    let book = gardyn_core::VarietyBook::starter();
    let entries: Vec<_> = SALAD_LOVER_KIT
        .iter()
        .map(|(slot, variety)| (SlotId(*slot), VarietyId::new(*variety), garden.created_at))
        .collect();
    store
        .plant_many(garden.id, &entries, garden.model.slot_count(), by)
        .await?;

    for planting in store.active_plantings(garden.id).await? {
        let Some(variety) = book.get(&planting.variety) else {
            continue;
        };
        let sprouted = gardyn_core::time::add_days(
            planting.planted_at,
            f64::from(variety.germination_days),
        );
        store
            .record_planting_event(
                garden.id,
                planting.id,
                gardyn_store::plantings::PlantingEvent::Germinated,
                sprouted,
            )
            .await?;
    }
    Ok(())
}

/// Overlay simulated sensors and canopy metrics onto a real state.
///
/// Only the physics is borrowed from the simulator. Metrics are matched to slots that
/// actually hold a plant, so a slot the operator emptied stops reporting canopy even
/// though the simulator is still growing something there.
pub fn overlay_telemetry(state: &mut GardenState, garden: &Garden, now: Timestamp) {
    let Some(simulated) = run(garden, now) else {
        return;
    };

    state.capabilities = simulated.capabilities;
    state.sensors = simulated.sensors;
    state.tank = simulated.tank;
    state.pump = simulated.pump;
    state.algae = simulated.algae;

    state.slot_metrics.clear();
    for slot in state.geometry.slots() {
        if state.planting_in(slot).is_none() {
            continue;
        }
        if let Some(metrics) = simulated.slot_metrics.get(&slot) {
            let mut metrics = metrics.clone();
            metrics.at = now;
            state.slot_metrics.insert(slot, metrics);
        }
    }
}

/// Run the physics forward from the garden's creation to now.
fn run(garden: &Garden, now: Timestamp) -> Option<GardenState> {
    if garden.model != DeviceModel::Simulated {
        return None;
    }

    // Seeded from the garden id so a given garden always tells the same story, and two
    // simulated gardens on one account look different from each other.
    let seed = (garden.id.as_uuid().as_u128() as u64) | 1;
    let mut sim = Simulation::new(seed, garden.created_at);
    sim.state.garden = garden.id;

    for (slot, variety) in SALAD_LOVER_KIT {
        sim.plant(SlotId(*slot), variety);
    }
    for capability in [Capability::WaterTemperature, Capability::CanopyMetrics] {
        sim.enable(capability);
    }

    let elapsed = gardyn_core::time::days_between(garden.created_at, now);
    let days = elapsed.clamp(1.0, MAX_SIMULATED_DAYS) as u32;

    // A "typical" operator, so the garden looks lived-in: mostly tended, with some
    // outstanding work. A perfectly maintained garden would make an empty dashboard.
    gardyn_sim::scenario::run(&mut sim, Operator::TYPICAL, days, seed);

    sim.state.now = now;
    Some(sim.state)
}

/// Render and store a frame for a simulated garden if the last one is stale.
///
/// Real gardens are left alone: their frames arrive from the edge agent, and
/// inventing a photograph of hardware we have never seen would be worse than showing
/// nothing.
pub async fn ensure_frame(
    store: &gardyn_store::Store,
    garden: &Garden,
    state: &GardenState,
    now: Timestamp,
) -> gardyn_store::Result<()> {
    if garden.model != DeviceModel::Simulated {
        return Ok(());
    }

    let due = match store.latest_frame(garden.id).await? {
        None => true,
        Some(latest) => {
            gardyn_core::time::days_between(latest.captured_at, now)
                > FRAME_INTERVAL_MINUTES / (24.0 * 60.0)
        }
    };
    if !due {
        return Ok(());
    }

    let bytes = match crate::render::render(state) {
        Ok(bytes) => bytes,
        Err(error) => {
            // A failed render must not take the dashboard down with it.
            tracing::warn!(%error, "could not render a simulated frame");
            return Ok(());
        }
    };

    let stored = store
        .put_frame(gardyn_store::frames::NewFrame {
            garden: garden.id,
            captured_at: now,
            width: crate::render::FRAME_WIDTH,
            height: crate::render::FRAME_HEIGHT,
            light_duty_milli: Some(crate::render::REFERENCE_DUTY_MILLI),
            // Rendered at the reference light level, which is what photo mode will do
            // on real hardware.
            comparable: true,
            source: gardyn_store::frames::FrameSource::Simulated,
            bytes: &bytes,
        })
        .await?;

    if let Err(rejected) = stored {
        tracing::warn!(%rejected, "simulated frame rejected");
    }
    Ok(())
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
    fn a_real_garden_gets_no_invented_telemetry() {
        let g = garden(DeviceModel::Studio2);
        let mut state = GardenState::for_garden(g.id, t0());
        state.capabilities = gardyn_core::CapabilitySet::empty();
        overlay_telemetry(&mut state, &g, add_days(t0(), 30.0));
        assert!(state.capabilities.is_empty());
        assert!(state.sensors.water_level_mm.is_none());
    }

    #[test]
    fn a_simulated_garden_gets_sensors() {
        let g = garden(DeviceModel::Simulated);
        let mut state = GardenState::for_garden(g.id, t0());
        overlay_telemetry(&mut state, &g, add_days(t0(), 60.0));
        assert!(state.sensors.water_level_mm.is_some());
        assert!(state.capabilities.contains(Capability::WaterTemperature));
    }

    #[test]
    fn canopy_is_only_reported_for_slots_that_hold_a_plant() {
        // The operator emptying a slot must stop it reporting, even though the
        // physics model is still happily growing something there.
        let g = garden(DeviceModel::Simulated);
        let mut state = GardenState::for_garden(g.id, t0());
        state.plantings.push(gardyn_core::Planting::new(
            gardyn_core::PlantingId(1),
            SlotId(0),
            VarietyId::new("kale-lacinato"),
            t0(),
        ));

        overlay_telemetry(&mut state, &g, add_days(t0(), 60.0));

        assert_eq!(state.slot_metrics.len(), 1);
        assert!(state.slot_metrics.contains_key(&SlotId(0)));
        assert!(
            !state.slot_metrics.contains_key(&SlotId(1)),
            "an empty slot reported canopy"
        );
    }

    #[test]
    fn the_same_garden_tells_the_same_story_on_every_page_load() {
        let g = garden(DeviceModel::Simulated);
        let at = add_days(t0(), 45.0);
        let mut first = GardenState::for_garden(g.id, t0());
        let mut second = GardenState::for_garden(g.id, t0());
        overlay_telemetry(&mut first, &g, at);
        overlay_telemetry(&mut second, &g, at);
        assert_eq!(first.tank.volume_l, second.tank.volume_l);
    }

    #[test]
    fn two_gardens_do_not_look_identical() {
        let at = add_days(t0(), 45.0);
        let (a, b) = (garden(DeviceModel::Simulated), garden(DeviceModel::Simulated));
        let mut sa = GardenState::for_garden(a.id, t0());
        let mut sb = GardenState::for_garden(b.id, t0());
        overlay_telemetry(&mut sa, &a, at);
        overlay_telemetry(&mut sb, &b, at);

        // Tank volume alone is a poor discriminator: two gardens topped up on the same
        // simulated day both sit at capacity. Compare a signal that carries per-tick
        // noise instead.
        assert_ne!(sa.sensors.pump_current_ma, sb.sensors.pump_current_ma);
    }

    #[test]
    fn a_brand_new_garden_does_not_panic_on_zero_elapsed_days() {
        let g = garden(DeviceModel::Simulated);
        let mut state = GardenState::for_garden(g.id, t0());
        overlay_telemetry(&mut state, &g, t0());
        assert!(state.sensors.water_level_mm.is_some());
    }
}
