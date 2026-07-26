//! Assembling the snapshot the rule engine reads.
//!
//! Plantings are authoritative and come from the database. Sensor readings are
//! overlaid on top when a garden has a source for them.
//!
//! The useful consequence: a garden with no hardware at all still gets real advice.
//! Thinning windows, harvest dates, root-check cadence and end-of-life replanting all
//! derive from the variety book and a planting date, so the system earns its keep
//! before anyone opens the device with a screwdriver.

use gardyn_core::{
    Capability, CapabilitySet, DeviceModel, Garden, GardenState, Geometry, SensorSnapshot,
    TankGeometry,
};
use gardyn_store::Store;
use jiff::Timestamp;

/// Physical layout for a model.
///
/// The Home line is three columns of ten; the Studio line is the two-by-eight working
/// assumption recorded in DESIGN.md, still unconfirmed pending Phase 0.
pub fn geometry_for(model: DeviceModel) -> Geometry {
    match model {
        DeviceModel::Home4 | DeviceModel::Home3 => Geometry {
            columns: 3,
            rows_per_column: 10,
        },
        _ => Geometry::STUDIO_2,
    }
}

/// Build the current state of a garden.
pub async fn build(
    store: &Store,
    garden: &Garden,
    now: Timestamp,
) -> gardyn_store::Result<GardenState> {
    let mut state = GardenState::for_garden(garden.id, now);
    state.geometry = geometry_for(garden.model);
    state.tank_geometry = TankGeometry::STUDIO_2;
    state.plantings = store.active_plantings(garden.id).await?;

    if garden.model == DeviceModel::Simulated {
        crate::demo::overlay_telemetry(&mut state, garden, now);
        return Ok(state);
    }

    // What no sensor can tell us: when the tank was last fed, conditioned or
    // scrubbed. Folded forward from the recorded actions.
    state.tank = store
        .tank_state_at(garden.id, &state.tank_geometry, now)
        .await?;

    state.slot_metrics.clear();
    match store.latest_reading(garden.id).await? {
        Some(sensors) => {
            // Capabilities come from what actually read back, not from configuration.
            // A probe that failed this morning drops out of the set on its own, and
            // the fallback rules resume without anyone touching a setting.
            state.capabilities = sensors.capabilities();

            if let Some(mm) = sensors.water_level_mm {
                state.tank.volume_l = state.tank_geometry.volume_from_distance(mm);
            }
            if let Some(rate) = store
                .fitted_consumption_lpd(
                    garden.id,
                    &state.tank_geometry,
                    gardyn_core::time::add_days(now, -CONSUMPTION_WINDOW_DAYS),
                    now,
                )
                .await?
            {
                state.tank.consumption_lpd = rate;
            }
            if let Some(ma) = sensors.pump_current_ma {
                state.pump.current_ma_ewma = ma;
            }

            state.sensors = sensors;
        }
        None => {
            // Nothing has ever reported. Claiming stock capabilities here would let
            // sensor-backed rules run against readings that do not exist.
            state.capabilities = CapabilitySet::empty();
            state.sensors = SensorSnapshot::empty(now);
        }
    }

    overlay_vision(store, garden, &mut state, now).await?;
    Ok(state)
}

/// Add whatever the camera measured, and the capabilities that follow from it.
///
/// Capabilities are derived from the measurements rather than from a setting, exactly
/// as they are for sensors. A camera that stopped reporting, a calibration that was
/// cleared, or a run of frames too dark to classify all produce the same thing — no
/// recent metrics — and the canopy rules stand down on their own.
async fn overlay_vision(
    store: &Store,
    garden: &Garden,
    state: &mut GardenState,
    now: Timestamp,
) -> gardyn_store::Result<()> {
    let metrics = store.latest_slot_metrics(garden.id).await?;
    let cutoff = gardyn_core::time::add_days(now, -VISION_STALE_DAYS);

    let mut fresh = 0usize;
    let mut segmented = 0usize;
    let mut diagnosed = 0usize;
    for m in metrics {
        if m.at < cutoff {
            continue;
        }
        fresh += 1;
        if m.plant_count.is_some() {
            segmented += 1;
        }
        if m.diagnosis.is_some() {
            diagnosed += 1;
        }
        state.slot_metrics.insert(m.slot, m);
    }

    if fresh > 0 {
        state.capabilities.insert(Capability::CanopyMetrics);
    }
    if segmented > 0 {
        state.capabilities.insert(Capability::PlantSegmentation);
    }
    if diagnosed > 0 {
        state.capabilities.insert(Capability::VisualDiagnosis);
    }

    if let Some(algae) = store.latest_algae(garden.id).await?
        && algae.at >= cutoff
    {
        state.algae = Some(algae);
    }

    Ok(())
}

/// How old the newest camera measurement may be before vision counts as absent.
///
/// Two days rather than two hours: a camera that misses a night is not a camera that
/// has failed, and flapping the capability on and off would flip the harvest rule
/// between measured and calendar every time a frame came out dark.
const VISION_STALE_DAYS: f64 = 2.0;

/// How far back to look when fitting the water-consumption rate.
///
/// Long enough to average out a single heavy watering day, short enough that a plant
/// pulled last month is not still inflating the estimate.
const CONSUMPTION_WINDOW_DAYS: f64 = 14.0;

/// Whether anything is actually measuring this garden.
pub fn has_telemetry(state: &GardenState) -> bool {
    !state.capabilities.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gardyn_auth::EmailAddress;
    use gardyn_core::{SlotId, VarietyId};

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    async fn fixture(model: DeviceModel) -> (Store, Garden) {
        let store = Store::in_memory().await.unwrap();
        let user = store
            .create_user(
                EmailAddress::parse("phil@example.com").unwrap(),
                "Phil",
                "a long enough password",
                t0(),
            )
            .await
            .unwrap();
        let garden = store
            .create_garden("Kitchen", model, "UTC", user.id, t0())
            .await
            .unwrap();
        (store, garden)
    }

    #[test]
    fn the_home_line_is_taller_than_the_studio_line() {
        assert_eq!(geometry_for(DeviceModel::Studio2).slot_count(), 16);
        assert_eq!(geometry_for(DeviceModel::Home4).slot_count(), 30);
    }

    #[tokio::test]
    async fn a_real_garden_reports_no_capabilities_until_something_measures_it() {
        let (store, garden) = fixture(DeviceModel::Studio2).await;
        let state = build(&store, &garden, t0()).await.unwrap();

        assert!(!has_telemetry(&state));
        assert!(state.capabilities.is_empty());
        assert!(state.sensors.water_level_mm.is_none());
    }

    #[tokio::test]
    async fn stored_plantings_reach_the_rule_engine() {
        let (store, garden) = fixture(DeviceModel::Studio2).await;
        store
            .plant(
                garden.id,
                SlotId(2),
                &VarietyId::new("kale-lacinato"),
                t0(),
                16,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        let state = build(&store, &garden, t0()).await.unwrap();
        assert_eq!(state.occupied_slots(), 1);
        assert_eq!(state.planting_in(SlotId(2)).unwrap().slot, SlotId(2));
        assert_eq!(state.planted().count(), 1, "variety should resolve");
    }

    #[tokio::test]
    async fn a_sensorless_garden_still_produces_calendar_advice() {
        // The whole point of separating plantings from telemetry: useful output with
        // no hardware whatsoever.
        let (store, garden) = fixture(DeviceModel::Studio2).await;
        // Gardyn puts Lacinato Kale at 58 days from germination to first harvest, so
        // this one is comfortably overdue.
        let planted = gardyn_core::time::add_days(t0(), -90.0);
        store
            .plant(
                garden.id,
                SlotId(0),
                &VarietyId::new("kale-lacinato"),
                planted,
                16,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        store
            .record_planting_event(
                garden.id,
                gardyn_core::PlantingId(1),
                gardyn_store::plantings::PlantingEvent::Germinated,
                gardyn_core::time::add_days(t0(), -76.0),
            )
            .await
            .unwrap();

        let state = build(&store, &garden, t0()).await.unwrap();
        let evaluation = gardyn_rules::default_engine().evaluate(&state);

        assert!(
            !evaluation.tasks.is_empty(),
            "a garden with plants but no sensors should still have advice"
        );
        // Kale first harvest is 35 days after germination, so it is overdue.
        assert!(evaluation.has(gardyn_core::TaskKind::Harvest));
    }

    #[tokio::test]
    async fn removing_a_planting_takes_it_out_of_the_snapshot() {
        let (store, garden) = fixture(DeviceModel::Studio2).await;
        let planting = store
            .plant(
                garden.id,
                SlotId(4),
                &VarietyId::new("arugula"),
                t0(),
                16,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(build(&store, &garden, t0()).await.unwrap().occupied_slots(), 1);
        store
            .remove_planting(garden.id, planting.id, t0())
            .await
            .unwrap();
        assert_eq!(build(&store, &garden, t0()).await.unwrap().occupied_slots(), 0);
    }

    /// Record one slot measurement against a real frame row.
    async fn measure(
        store: &Store,
        garden: &Garden,
        slot: SlotId,
        at: Timestamp,
        area: f32,
        segmented: bool,
    ) {
        const PNG: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
            0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let frame = store
            .put_frame(gardyn_store::frames::NewFrame {
                garden: garden.id,
                captured_at: at,
                width: 1,
                height: 1,
                light_duty_milli: Some(800),
                comparable: true,
                source: gardyn_store::frames::FrameSource::Agent,
                bytes: PNG,
            })
            .await
            .unwrap()
            .unwrap();

        let mut metrics = gardyn_core::SlotMetrics::new(slot, at, area);
        metrics.green_fraction = 0.7;
        if segmented {
            metrics.plant_count = Some(3);
        }
        store
            .record_slot_metrics(garden.id, frame.id, &[metrics])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_recorded_tank_action_reaches_the_rule_engine() {
        // Before the tank event log existed, `last_deep_clean` was always empty on a
        // real garden, so the maintenance rules fired on the first tick and never
        // stopped — completing the task changed nothing they read.
        let (store, garden) = fixture(DeviceModel::Studio2).await;
        store
            .plant(
                garden.id,
                SlotId(0),
                &VarietyId::new("basil"),
                gardyn_core::time::add_days(t0(), -10.0),
                16,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        let before = build(&store, &garden, t0()).await.unwrap();
        assert_eq!(before.tank.last_deep_clean, None);

        store
            .record_tank_event(garden.id, gardyn_core::TankEvent::DeepClean, None, t0())
            .await
            .unwrap();

        let after = build(&store, &garden, t0()).await.unwrap();
        assert_eq!(after.tank.last_deep_clean, Some(t0()));
        assert!(after.tank.days_since_deep_clean(t0()) < 1.0);
    }

    #[tokio::test]
    async fn feeding_the_tank_silences_the_feeding_task() {
        // End to end: the rule fires, the action is recorded, the rule stands down.
        let (store, garden) = fixture(DeviceModel::Studio2).await;
        store
            .plant(
                garden.id,
                SlotId(0),
                &VarietyId::new("basil"),
                gardyn_core::time::add_days(t0(), -30.0),
                16,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        // Food during germination hinders it, so the dosing rule stays silent until
        // something has actually come up.
        store
            .record_planting_event(
                garden.id,
                gardyn_core::PlantingId(1),
                gardyn_store::plantings::PlantingEvent::Germinated,
                gardyn_core::time::add_days(t0(), -20.0),
            )
            .await
            .unwrap();

        let engine = gardyn_rules::default_engine();
        let before = engine.evaluate(&build(&store, &garden, t0()).await.unwrap());
        assert!(
            before.has(gardyn_core::TaskKind::AddPlantFood),
            "a never-fed tank should be asking to be fed"
        );

        store
            .record_tank_event(
                garden.id,
                gardyn_core::TankEvent::FedToStrength { strength: 1.0 },
                None,
                t0(),
            )
            .await
            .unwrap();

        let after = engine.evaluate(&build(&store, &garden, t0()).await.unwrap());
        assert!(
            !after.has(gardyn_core::TaskKind::AddPlantFood),
            "the tank was just fed; the task should be gone"
        );
    }

    #[tokio::test]
    async fn camera_measurements_light_up_the_vision_capabilities() {
        let (store, garden) = fixture(DeviceModel::Studio2).await;
        assert!(
            !build(&store, &garden, t0())
                .await
                .unwrap()
                .capabilities
                .contains(Capability::CanopyMetrics)
        );

        measure(&store, &garden, SlotId(0), t0(), 240.0, false).await;
        let state = build(&store, &garden, t0()).await.unwrap();
        assert!(state.capabilities.contains(Capability::CanopyMetrics));
        assert!(!state.capabilities.contains(Capability::PlantSegmentation));
        assert_eq!(state.metrics_for(SlotId(0)).unwrap().canopy_area_cm2, 240.0);

        measure(&store, &garden, SlotId(1), t0(), 180.0, true).await;
        let state = build(&store, &garden, t0()).await.unwrap();
        assert!(state.capabilities.contains(Capability::PlantSegmentation));
    }

    #[tokio::test]
    async fn a_camera_that_stopped_reporting_drops_the_capability_by_itself() {
        // Same contract as a failed probe: capabilities come from measurements, so
        // nothing has to be switched off when the camera goes quiet.
        let (store, garden) = fixture(DeviceModel::Studio2).await;
        measure(&store, &garden, SlotId(0), t0(), 240.0, true).await;

        let later = gardyn_core::time::add_days(t0(), 5.0);
        let state = build(&store, &garden, later).await.unwrap();
        assert!(!state.capabilities.contains(Capability::CanopyMetrics));
        assert!(state.metrics_for(SlotId(0)).is_none());
    }

    #[tokio::test]
    async fn one_dark_night_does_not_flap_the_capability_off() {
        // Two days of tolerance, so a single unusable frame does not flip the harvest
        // rule between measured and calendar.
        let (store, garden) = fixture(DeviceModel::Studio2).await;
        measure(&store, &garden, SlotId(0), t0(), 240.0, false).await;

        let tomorrow = gardyn_core::time::add_days(t0(), 1.0);
        let state = build(&store, &garden, tomorrow).await.unwrap();
        assert!(state.capabilities.contains(Capability::CanopyMetrics));
    }

    #[tokio::test]
    async fn measured_harvest_supersedes_the_calendar_once_the_camera_is_calibrated() {
        // The payoff for the whole vision pipeline: the same garden, the same plant,
        // and a different rule answering for it.
        let (store, garden) = fixture(DeviceModel::Studio2).await;
        let planted = gardyn_core::time::add_days(t0(), -20.0);
        store
            .plant(
                garden.id,
                SlotId(0),
                &VarietyId::new("kale-lacinato"),
                planted,
                16,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        store
            .record_planting_event(
                garden.id,
                gardyn_core::PlantingId(1),
                gardyn_store::plantings::PlantingEvent::Germinated,
                gardyn_core::time::add_days(t0(), -14.0),
            )
            .await
            .unwrap();

        let before = gardyn_rules::default_engine()
            .evaluate(&build(&store, &garden, t0()).await.unwrap());
        assert!(before.was_suppressed("harvest-by-canopy"));

        measure(&store, &garden, SlotId(0), t0(), 900.0, false).await;
        let after = gardyn_rules::default_engine()
            .evaluate(&build(&store, &garden, t0()).await.unwrap());

        assert!(!after.was_suppressed("harvest-by-canopy"));
        assert!(after.was_suppressed("harvest-by-calendar"));
    }

    #[tokio::test]
    async fn a_simulated_garden_gets_telemetry_overlaid() {
        let (store, garden) = fixture(DeviceModel::Simulated).await;
        let state = build(&store, &garden, gardyn_core::time::add_days(t0(), 40.0))
            .await
            .unwrap();
        assert!(has_telemetry(&state));
        assert!(state.sensors.water_level_mm.is_some());
    }
}
