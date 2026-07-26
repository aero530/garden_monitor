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
    CapabilitySet, DeviceModel, Garden, GardenState, Geometry, SensorSnapshot, TankGeometry,
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

    Ok(state)
}

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
