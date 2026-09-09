//! Running the current rule set against what actually happened.
//!
//! The rules are pure functions of a `GardenState`, and every input to that state is
//! stored with a timestamp. So the state can be rebuilt as it stood on any past day
//! and the rules asked what they *would* have said — which is the only honest way to
//! evaluate a threshold change. Adjusting a constant and waiting a month to see what
//! happens is not a test.
//!
//! It also answers the hardware question directly. Replay the same history with
//! `--capability conductivity` and the measured dosing rule takes over from the
//! estimate; the difference in what you would have been told is what the probe is
//! worth on *your* garden rather than in the simulator.

use garden_core::{
    Capability, CapabilitySet, GardenId, GardenState, Geometry, SensorSnapshot, TankGeometry,
    TaskKind, Timestamp, time::add_days,
};
use garden_rules::Engine;
use garden_store::Store;
use std::collections::BTreeMap;

/// What one replayed day produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Day {
    pub at: Timestamp,
    /// Tasks that were not outstanding the day before.
    pub new_tasks: Vec<(TaskKind, String)>,
    pub outstanding: usize,
    /// Whether any sensor reading existed for this day at all.
    pub had_telemetry: bool,
}

/// One outstanding task, flattened for printing.
#[derive(Debug, Clone, PartialEq)]
pub struct Outstanding {
    pub kind: TaskKind,
    pub key: String,
    pub target: String,
    pub severity: garden_core::Severity,
    pub rationale: String,
    /// Days until the late edge of the due window. Negative means overdue.
    pub due_in_days: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub days: Vec<Day>,
    pub first_seen: BTreeMap<TaskKind, Timestamp>,
    pub totals: BTreeMap<TaskKind, usize>,
    /// Days with no sensor reading. Reported because a replay over a gap is mostly
    /// measuring the gap.
    pub blind_days: usize,
    /// What the rules still say on the last replayed day.
    ///
    /// The number the rest of this summary does not give you. `totals` counts *raise
    /// events* — a task that appeared on day 0 and never resolved counts once, and one
    /// that flapped six times counts six — so a large total is equally consistent with
    /// a garden that needs a lot of work and one that needs none. Comparing it against
    /// the `tasks` table, which holds only what is outstanding, is comparing a
    /// stopwatch to a thermometer.
    pub outstanding_now: Vec<Outstanding>,
    /// Rules that could not run on the last day, already explained.
    pub suppressed_now: Vec<String>,
    /// Capabilities the last day's snapshot actually had.
    pub capabilities_now: Vec<&'static str>,
}

impl Summary {
    pub fn total_tasks(&self) -> usize {
        self.totals.values().sum()
    }
}

/// Replay `days` of history, one evaluation per day.
///
/// Daily rather than every five minutes on purpose. The rules are re-entrant and
/// mostly move on the scale of days; 288 evaluations per day would take 288 times as
/// long to tell you the same thing.
pub async fn run(
    store: &Store,
    garden: GardenId,
    geometry: Geometry,
    engine: &Engine,
    from: Timestamp,
    to: Timestamp,
    extra: &[Capability],
) -> garden_store::Result<Summary> {
    let mut summary = Summary::default();
    let mut previous: Vec<String> = Vec::new();

    let span = garden_core::time::days_between(from, to).max(0.0);
    let steps = span.ceil() as u32;

    for step in 0..=steps {
        let at = add_days(from, f64::from(step));
        if at > to {
            break;
        }

        let state = state_at(store, garden, geometry, at, extra).await?;
        let evaluation = engine.evaluate(&state);

        let keys: Vec<String> = evaluation.tasks.iter().map(|t| t.key.0.clone()).collect();
        let new_tasks: Vec<(TaskKind, String)> = evaluation
            .tasks
            .iter()
            .filter(|t| !previous.contains(&t.key.0))
            .map(|t| (t.kind, t.rationale.clone()))
            .collect();

        for (kind, _) in &new_tasks {
            summary.first_seen.entry(*kind).or_insert(at);
            *summary.totals.entry(*kind).or_default() += 1;
        }

        let had_telemetry = !state.capabilities.is_empty();
        if !had_telemetry {
            summary.blind_days += 1;
        }

        summary.days.push(Day {
            at,
            new_tasks,
            outstanding: evaluation.tasks.len(),
            had_telemetry,
        });

        // Overwritten each step, so what survives is the last day evaluated — the one
        // that should agree with the `tasks` table and the web UI.
        summary.outstanding_now = evaluation
            .tasks
            .iter()
            .map(|t| Outstanding {
                kind: t.kind,
                key: t.key.0.clone(),
                target: t.target.to_string(),
                severity: t.severity,
                rationale: t.rationale.clone(),
                due_in_days: garden_core::time::days_between(at, t.due.latest),
            })
            .collect();
        summary.suppressed_now = evaluation.suppressed.iter().map(|s| s.explain()).collect();
        summary.capabilities_now = state.capabilities.iter().map(|c| c.label()).collect();

        previous = keys;
    }

    Ok(summary)
}

/// Rebuild the garden as it stood at `at`.
///
/// Deliberately a separate function from the web server's `state::build`, and
/// deliberately not sharing it: that one always means *now* and takes shortcuts a
/// replay must not, like using the newest reading regardless of age.
async fn state_at(
    store: &Store,
    garden: GardenId,
    geometry: Geometry,
    at: Timestamp,
    extra: &[Capability],
) -> garden_store::Result<GardenState> {
    let mut state = GardenState::for_garden(garden, at);
    state.geometry = geometry;
    state.tank_geometry = TankGeometry::STUDIO_2;

    // Plantings as they were: planted by then, and not yet pulled.
    state.plantings = store
        .all_plantings(garden)
        .await?
        .into_iter()
        .filter(|p| p.planted_at <= at && p.removed_at.is_none_or(|removed| removed > at))
        .map(|mut p| {
            // A germination recorded after this moment had not happened yet. Leaving
            // it in would make the plant look older than it was and fire harvest tasks
            // days early — the exact error a replay exists to catch, not commit.
            if p.germinated_at.is_some_and(|g| g > at) {
                p.germinated_at = None;
            }
            p
        })
        .collect();

    state.tank = store.tank_state_at(garden, &state.tank_geometry, at).await?;

    // The most recent reading at or before this moment, if it is recent enough to
    // still describe the garden.
    let window_start = add_days(at, -READING_STALE_DAYS);
    let readings = store.readings_between(garden, window_start, at).await?;
    match readings.last() {
        Some(sensors) => {
            state.capabilities = sensors.capabilities();
            if let Some(mm) = sensors.water_level_mm {
                state.tank.volume_l = state.tank_geometry.volume_from_distance(mm);
            }
            if let Some(rate) = store
                .fitted_consumption_lpd(
                    garden,
                    &state.tank_geometry,
                    add_days(at, -CONSUMPTION_WINDOW_DAYS),
                    at,
                )
                .await?
            {
                state.tank.consumption_lpd = rate;
            }
            if let Some(ma) = sensors.pump_current_ma {
                state.pump.current_ma_ewma = ma;
            }
            state.sensors = sensors.clone();
        }
        None => {
            state.capabilities = CapabilitySet::empty();
            state.sensors = SensorSnapshot::empty(at);
        }
    }

    // Vision, as of that day.
    for metrics in store.latest_slot_metrics(garden).await? {
        if metrics.at <= at && metrics.at >= add_days(at, -VISION_STALE_DAYS) {
            state.capabilities.insert(Capability::CanopyMetrics);
            if metrics.plant_count.is_some() {
                state.capabilities.insert(Capability::PlantSegmentation);
            }
            state.slot_metrics.insert(metrics.slot, metrics);
        }
    }

    // Hardware you are considering buying. Added last so it cannot be undone by the
    // derivation above, which is the whole point of the flag.
    for capability in extra {
        state.capabilities.insert(*capability);
    }

    Ok(state)
}

/// A reading older than this no longer describes the garden.
const READING_STALE_DAYS: f64 = 1.0;
const VISION_STALE_DAYS: f64 = 2.0;
const CONSUMPTION_WINDOW_DAYS: f64 = 14.0;

/// Parse a capability name from the command line.
pub fn parse_capability(name: &str) -> Option<Capability> {
    let normalised = name.to_lowercase().replace(['-', '_'], " ");
    [
        Capability::AirTemperature,
        Capability::AirHumidity,
        Capability::WaterLevel,
        Capability::PumpCurrent,
        Capability::PcbTemperature,
        Capability::WaterTemperature,
        Capability::Conductivity,
        Capability::PotentialHydrogen,
        Capability::CanopyMetrics,
        Capability::PlantSegmentation,
        Capability::VisualDiagnosis,
        Capability::LightControl,
        Capability::PumpControl,
    ]
    .into_iter()
    .find(|c| {
        c.label().to_lowercase() == normalised
            || format!("{c:?}").to_lowercase() == normalised.replace(' ', "")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use garden_auth::EmailAddress;
    use garden_core::{DeviceModel, SlotId, TankEvent, VarietyId};

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    async fn fixture() -> (Store, GardenId) {
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
            .create_garden("Kitchen", DeviceModel::Studio2, "UTC", user.id, t0())
            .await
            .unwrap();
        (store, garden.id)
    }

    async fn plant_kale(store: &Store, garden: GardenId, planted: Timestamp) {
        store
            .plant(garden, SlotId(0), &VarietyId::new("kale-lacinato"), planted, 16, None)
            .await
            .unwrap()
            .unwrap();
        store
            .record_planting_event(
                garden,
                garden_core::PlantingId(1),
                garden_store::plantings::PlantingEvent::Germinated,
                add_days(planted, 6.0),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_empty_garden_produces_no_tasks() {
        let (store, garden) = fixture().await;
        let summary = run(
            &store,
            garden,
            Geometry::STUDIO_2,
            &garden_rules::default_engine(),
            t0(),
            add_days(t0(), 30.0),
            &[],
        )
        .await
        .unwrap();

        assert_eq!(summary.days.len(), 31);
        assert_eq!(summary.total_tasks(), 0);
        assert_eq!(summary.blind_days, 31, "no sensor ever reported");
    }

    #[tokio::test]
    async fn a_planting_produces_a_harvest_task_on_the_day_it_matures() {
        let (store, garden) = fixture().await;
        plant_kale(&store, garden, t0()).await;

        let summary = run(
            &store,
            garden,
            Geometry::STUDIO_2,
            &garden_rules::default_engine(),
            t0(),
            add_days(t0(), 80.0),
            &[],
        )
        .await
        .unwrap();

        let first = summary
            .first_seen
            .get(&TaskKind::Harvest)
            .expect("kale should become harvestable");
        let day = garden_core::time::days_between(t0(), *first);
        // Gardyn publishes 65 days to maturity from sowing and 7-21 to sprout, so the
        // book carries 58 days from germination. Germination was recorded on day 6.
        assert!((60.0..=68.0).contains(&day), "harvest appeared on day {day}");
    }

    #[tokio::test]
    async fn a_germination_recorded_later_does_not_leak_backwards() {
        // The failure this guards: using today's germination date at every step makes
        // the plant look older than it was and fires harvest days early. A replay that
        // did that would be worse than useless — it would be confidently wrong.
        let (store, garden) = fixture().await;
        plant_kale(&store, garden, t0()).await;

        let summary = run(
            &store,
            garden,
            Geometry::STUDIO_2,
            &garden_rules::default_engine(),
            t0(),
            add_days(t0(), 5.0),
            &[],
        )
        .await
        .unwrap();

        // Germination is on day 6, so nothing in the first five days may depend on it.
        assert_eq!(summary.first_seen.get(&TaskKind::Harvest), None);
        assert_eq!(summary.first_seen.get(&TaskKind::Thin), None);
    }

    #[tokio::test]
    async fn a_pulled_plant_stops_producing_tasks_from_the_day_it_was_pulled() {
        let (store, garden) = fixture().await;
        plant_kale(&store, garden, t0()).await;
        store
            .remove_planting(garden, garden_core::PlantingId(1), add_days(t0(), 20.0))
            .await
            .unwrap();

        let summary = run(
            &store,
            garden,
            Geometry::STUDIO_2,
            &garden_rules::default_engine(),
            t0(),
            add_days(t0(), 60.0),
            &[],
        )
        .await
        .unwrap();

        assert!(summary.days.iter().take(20).any(|d| d.outstanding > 0));
        assert!(
            summary.days.iter().skip(25).all(|d| d.outstanding == 0),
            "a pulled plant should stop generating work"
        );
    }

    #[tokio::test]
    async fn a_high_raise_count_can_still_end_with_nothing_outstanding() {
        // Why `outstanding_now` exists at all. `totals` counts raise events over the
        // window; the `tasks` table holds only what is open. Reading a large total as
        // "so the table should have rows" sent an evening chasing a dispatcher bug
        // that was not there, so the two numbers now appear side by side and the
        // report says which is which.
        let (store, garden) = fixture().await;
        plant_kale(&store, garden, t0()).await;
        store
            .remove_planting(garden, garden_core::PlantingId(1), add_days(t0(), 20.0))
            .await
            .unwrap();

        let summary = run(
            &store,
            garden,
            Geometry::STUDIO_2,
            &garden_rules::default_engine(),
            t0(),
            add_days(t0(), 60.0),
            &[],
        )
        .await
        .unwrap();

        assert!(summary.total_tasks() > 0, "work was raised while it grew");
        assert!(
            summary.outstanding_now.is_empty(),
            "and none of it is open now: {:?}",
            summary.outstanding_now
        );
    }

    #[tokio::test]
    async fn outstanding_now_is_the_last_day_and_carries_its_reasons() {
        let (store, garden) = fixture().await;
        // Comfortably past first harvest, so something is certainly open.
        plant_kale(&store, garden, add_days(t0(), -90.0)).await;

        let summary = run(
            &store,
            garden,
            Geometry::STUDIO_2,
            &garden_rules::default_engine(),
            t0(),
            add_days(t0(), 2.0),
            &[],
        )
        .await
        .unwrap();

        assert_eq!(
            summary.outstanding_now.len(),
            summary.days.last().unwrap().outstanding,
            "outstanding_now must describe the last day, not an earlier one"
        );
        assert!(summary.outstanding_now.iter().any(|t| t.kind == TaskKind::Harvest));
        assert!(
            summary.outstanding_now.iter().all(|t| !t.rationale.is_empty()),
            "every task must say why"
        );
        // No sensor ever reported here, so the sensor-backed rules must account for
        // themselves rather than vanish silently.
        assert!(summary.capabilities_now.is_empty());
        assert!(
            !summary.suppressed_now.is_empty(),
            "a blind garden should list the rules that stood down"
        );
    }

    #[tokio::test]
    async fn the_tank_is_reconstructed_as_of_each_day() {
        let (store, garden) = fixture().await;
        plant_kale(&store, garden, t0()).await;
        store
            .record_tank_event(
                garden,
                TankEvent::FedToStrength { strength: 1.0 },
                None,
                add_days(t0(), 30.0),
            )
            .await
            .unwrap();

        let engine = garden_rules::default_engine();
        let before = run(
            &store,
            garden,
            Geometry::STUDIO_2,
            &engine,
            add_days(t0(), 20.0),
            add_days(t0(), 21.0),
            &[],
        )
        .await
        .unwrap();
        let after = run(
            &store,
            garden,
            Geometry::STUDIO_2,
            &engine,
            add_days(t0(), 31.0),
            add_days(t0(), 32.0),
            &[],
        )
        .await
        .unwrap();

        assert!(before.totals.contains_key(&TaskKind::AddPlantFood));
        assert!(!after.totals.contains_key(&TaskKind::AddPlantFood));
    }

    #[tokio::test]
    async fn a_probe_that_reports_changes_what_you_are_told() {
        // The hardware question answered against your own history: two identical
        // gardens, one with an EC probe reporting, and the difference in what each
        // would have interrupted you about.
        //
        // Note there is no `--capability` flag here. A probe that reports *derives*
        // its capability from the reading, which is the whole point of the capability
        // model — the flag is for asking about hardware you do not have.
        let (store, blind) = fixture().await;
        let user = store
            .create_user(
                EmailAddress::parse("other@example.com").unwrap(),
                "Other",
                "a long enough password",
                t0(),
            )
            .await
            .unwrap();
        let probed = store
            .create_garden("Probed", DeviceModel::Studio2, "UTC", user.id, t0())
            .await
            .unwrap()
            .id;

        plant_kale(&store, blind, t0()).await;
        store
            .plant(probed, SlotId(0), &VarietyId::new("kale-lacinato"), t0(), 16, None)
            .await
            .unwrap()
            .unwrap();
        store
            .record_planting_event(
                probed,
                garden_core::PlantingId(2),
                garden_store::plantings::PlantingEvent::Germinated,
                add_days(t0(), 6.0),
            )
            .await
            .unwrap();

        for day in 0..=60 {
            let at = add_days(t0(), f64::from(day));
            let mut plain = SensorSnapshot::empty(at);
            plain.water_level_mm = Some(150.0);
            store.record_reading(blind, &plain, None).await.unwrap();

            let mut measured = plain.clone();
            // Comfortably in band, which the volume estimate has no way to know.
            measured.ec_ms_cm = Some(1.6);
            store.record_reading(probed, &measured, None).await.unwrap();
        }

        let engine = garden_rules::default_engine();
        let window = (t0(), add_days(t0(), 60.0));
        let without = run(&store, blind, Geometry::STUDIO_2, &engine, window.0, window.1, &[])
            .await
            .unwrap();
        let with = run(&store, probed, Geometry::STUDIO_2, &engine, window.0, window.1, &[])
            .await
            .unwrap();

        assert_eq!(without.blind_days, 0, "readings were recorded every day");
        assert!(
            without.totals.contains_key(&TaskKind::AddPlantFood),
            "the volume estimate should keep asking to be fed"
        );
        assert!(
            !with.totals.contains_key(&TaskKind::AddPlantFood),
            "a measured, in-band tank should not be asking for food"
        );
    }

    #[tokio::test]
    async fn a_capability_with_no_reading_behind_it_falls_back_rather_than_going_silent() {
        // The superset invariant, seen from the replay side. Declaring an EC probe you
        // have not wired up must not silence dosing altogether — the measured rule
        // keeps the estimate's logic for exactly this case.
        let (store, garden) = fixture().await;
        plant_kale(&store, garden, t0()).await;

        let with_probe = run(
            &store,
            garden,
            Geometry::STUDIO_2,
            &garden_rules::default_engine(),
            t0(),
            add_days(t0(), 60.0),
            &[Capability::Conductivity],
        )
        .await
        .unwrap();
        assert!(with_probe.totals.contains_key(&TaskKind::AddPlantFood));
    }

    #[tokio::test]
    async fn a_zero_length_range_still_evaluates_once() {
        let (store, garden) = fixture().await;
        let summary = run(
            &store,
            garden,
            Geometry::STUDIO_2,
            &garden_rules::default_engine(),
            t0(),
            t0(),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(summary.days.len(), 1);
    }

    #[test]
    fn capability_names_are_forgiving_about_punctuation() {
        assert_eq!(parse_capability("conductivity"), Some(Capability::Conductivity));
        assert_eq!(
            parse_capability("water-temperature"),
            Some(Capability::WaterTemperature)
        );
        assert_eq!(
            parse_capability("canopy_metrics"),
            Some(Capability::CanopyMetrics)
        );
        assert_eq!(parse_capability("EC probe"), Some(Capability::Conductivity));
        assert_eq!(parse_capability("nonsense"), None);
    }
}
