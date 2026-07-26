//! A simulated Gardyn, so the brain can be built without the hardware.
//!
//! This is the reason the whole system is trait-based. Rules, notification policy,
//! escalation, and the dashboard are the bulk of the work, and none of it needs a
//! Raspberry Pi. Here a season runs in milliseconds, deterministically, and a
//! threshold change can be evaluated against months of behaviour rather than against
//! one hand-written snapshot.

pub mod physics;
pub mod scenario;

use gardyn_core::{
    Capability, GardenState, HarvestStyle, Planting, PlantingId, SlotId, Target, Task, TaskDetail,
    TaskKind, Timestamp, VarietyId,
};
use gardyn_hal::{HalError, SensorBank};
use physics::{Environment, Fouling, Lcg, PlantSim};

/// The garden, its physics, and its history.
pub struct Simulation {
    pub state: GardenState,
    pub env: Environment,
    plants: Vec<PlantSim>,
    fouling: Fouling,
    rng: Lcg,
    next_planting_id: u64,
    /// Cumulative harvested canopy area, as a stand-in for yield.
    pub harvested_cm2: f32,
    /// Days on which the tank was completely dry.
    pub dry_days: u32,
}

impl Simulation {
    pub fn new(seed: u64, now: Timestamp) -> Self {
        Self {
            state: GardenState::new_studio_2(now),
            env: Environment::default(),
            plants: Vec::new(),
            fouling: Fouling::CLEAN,
            rng: Lcg::new(seed),
            next_planting_id: 1,
            harvested_cm2: 0.0,
            dry_days: 0,
        }
    }

    /// Enable a capability, as if the hardware had been fitted or a vision stage
    /// switched on.
    pub fn enable(&mut self, capability: Capability) -> &mut Self {
        self.state.capabilities.insert(capability);
        self
    }

    pub fn fouling_level(&self) -> f32 {
        self.fouling.level
    }

    pub fn plant(&mut self, slot: SlotId, variety: &str) -> PlantingId {
        let id = PlantingId(self.next_planting_id);
        self.next_planting_id += 1;
        self.state.plantings.push(Planting::new(
            id,
            slot,
            VarietyId::new(variety),
            self.state.now,
        ));
        self.plants.push(PlantSim::new(id, slot, 0));
        id
    }

    /// Advance the world by `days`.
    pub fn tick(&mut self, days: f64) {
        let volume_before = self.state.tank.volume_l;

        physics::advance_clock(&mut self.state, days);
        physics::germinate(&mut self.state, &mut self.plants, &mut self.rng);

        let (transpired, uptake) = physics::grow(
            &mut self.plants,
            &mut self.state,
            &self.env,
            days,
            &mut self.rng,
        );

        self.state
            .tank
            .consume_water(transpired + self.env.evaporation_lpd * days as f32);
        self.state.tank.consume_nutrient(uptake);

        if self.state.tank.volume_l <= 0.05 {
            self.dry_days += 1;
        }

        let since_conditioner = self.state.tank.days_since_conditioner(self.state.now);
        self.fouling.advance(days, since_conditioner, &self.env);

        physics::sense(&mut self.state, &self.env, self.fouling, &mut self.rng);
        physics::observe(&mut self.state, &self.plants, &mut self.rng, &self.env);
        physics::update_consumption(&mut self.state, volume_before, days);

        self.apply_capability_mask();
    }

    /// Hide readings for hardware that is not fitted and vision stages that are off.
    ///
    /// The physics always computes everything; this mask is what makes the capability
    /// ladder testable, because the rules then see exactly what a real, partially
    /// equipped garden would show them.
    fn apply_capability_mask(&mut self) {
        let caps = self.state.capabilities.clone();
        let s = &mut self.state.sensors;

        if !caps.contains(Capability::AirTemperature) {
            s.air_temp_c = None;
        }
        if !caps.contains(Capability::AirHumidity) {
            s.humidity_pct = None;
        }
        if !caps.contains(Capability::PcbTemperature) {
            s.pcb_temp_c = None;
        }
        if !caps.contains(Capability::WaterLevel) {
            s.water_level_mm = None;
        }
        if !caps.contains(Capability::WaterTemperature) {
            s.water_temp_c = None;
        }
        if !caps.contains(Capability::PumpCurrent) {
            s.pump_current_ma = None;
        }
        if !caps.contains(Capability::Conductivity) {
            s.ec_ms_cm = None;
        }
        if !caps.contains(Capability::PotentialHydrogen) {
            s.ph = None;
        }

        let canopy = caps.contains(Capability::CanopyMetrics);
        let segmentation = caps.contains(Capability::PlantSegmentation);
        if !canopy && !segmentation {
            self.state.slot_metrics.clear();
        } else if !segmentation {
            // Canopy metrics without segmentation: no per-plant counts or flowering.
            for m in self.state.slot_metrics.values_mut() {
                m.plant_count = None;
                m.flowering = None;
            }
        }
    }

    /// Carry out a task, applying its physical effect.
    ///
    /// Returns false when the task has no simulated consequence (inspections,
    /// pollination), which still counts as done from the operator's point of view.
    pub fn perform(&mut self, task: &Task) -> bool {
        let now = self.state.now;
        let geometry = self.state.tank_geometry;

        match task.kind {
            TaskKind::AddWater => {
                let litres = match task.detail {
                    Some(TaskDetail::Water { litres }) => litres,
                    _ => geometry.capacity_l - self.state.tank.volume_l,
                };
                self.state.tank.top_off(litres, &geometry, now);
                true
            }
            TaskKind::AddPlantFood => {
                let ml = match task.detail {
                    Some(TaskDetail::Dose { millilitres }) => millilitres,
                    _ => 0.0,
                };
                // Normalise: `food_ml_per_litre` mL dissolved in one litre is one unit.
                let units = ml / self.state.dosing.food_ml_per_litre.max(f32::EPSILON);
                self.state.tank.add_food(units, now);
                true
            }
            TaskKind::AddConditioner => {
                self.state.tank.add_conditioner(now);
                self.fouling.level *= 0.85;
                true
            }
            TaskKind::PruneRoots => {
                self.mark_planting(task, |p, now| p.last_root_check = Some(now));
                // Clearing roots from the flow path recovers most of the restriction.
                self.fouling.level *= 0.55;
                true
            }
            TaskKind::PrunePlant => {
                self.mark_planting(task, |p, now| p.last_prune = Some(now));
                self.scale_canopy(task, 0.7);
                true
            }
            TaskKind::Harvest => {
                self.harvest(task);
                true
            }
            TaskKind::Thin => {
                self.mark_planting(task, |p, now| p.thinned_at = Some(now));
                if let Some(i) = self.plant_for(task) {
                    self.plants[i].sprouts = self.plants[i].sprouts.min(3);
                }
                true
            }
            TaskKind::TankRefresh => {
                self.state.tank.refresh(geometry.capacity_l, &geometry, now);
                self.fouling.level *= 0.4;
                true
            }
            TaskKind::DeepClean => {
                self.state.tank.deep_clean(now);
                self.fouling = Fouling::CLEAN;
                self.state.pump.rebaseline();
                true
            }
            TaskKind::Replant => {
                self.replant(task);
                true
            }
            TaskKind::Pollinate | TaskKind::Inspect => false,
        }
    }

    fn plant_for(&self, task: &Task) -> Option<usize> {
        let Target::Planting(id) = task.target else {
            return None;
        };
        self.plants.iter().position(|p| p.id == id)
    }

    fn mark_planting(&mut self, task: &Task, f: impl Fn(&mut Planting, Timestamp)) {
        let now = self.state.now;
        if let Target::Planting(id) = task.target
            && let Some(p) = self.state.plantings.iter_mut().find(|p| p.id == id)
        {
            f(p, now);
        }
    }

    fn scale_canopy(&mut self, task: &Task, factor: f32) {
        if let Some(i) = self.plant_for(task) {
            self.plants[i].canopy_cm2 *= factor;
        }
    }

    fn harvest(&mut self, task: &Task) {
        let now = self.state.now;
        let Target::Planting(id) = task.target else {
            return;
        };
        let Some(index) = self.plants.iter().position(|p| p.id == id) else {
            return;
        };
        let style = self
            .state
            .plantings
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| self.state.varieties.get(&p.variety))
            .map(|v| v.harvest_style);

        let before = self.plants[index].canopy_cm2;
        let keep = match style {
            Some(HarvestStyle::Single) => 0.0,
            Some(HarvestStyle::ContinuousFruiting { .. }) => 0.85,
            _ => 0.55,
        };
        let taken = before * (1.0 - keep);
        self.plants[index].canopy_cm2 = before * keep;
        self.plants[index].yielded_cm2 += taken;
        self.harvested_cm2 += taken;

        if let Some(p) = self.state.plantings.iter_mut().find(|p| p.id == id) {
            p.harvest_count += 1;
            p.last_harvest = Some(now);
            if matches!(style, Some(HarvestStyle::Single)) {
                p.removed_at = Some(now);
            }
        }
    }

    fn replant(&mut self, task: &Task) {
        let now = self.state.now;
        let Target::Planting(id) = task.target else {
            return;
        };
        let Some((slot, variety)) = self
            .state
            .plantings
            .iter()
            .find(|p| p.id == id)
            .map(|p| (p.slot, p.variety.clone()))
        else {
            return;
        };

        if let Some(p) = self.state.plantings.iter_mut().find(|p| p.id == id) {
            p.removed_at = Some(now);
        }
        self.plants.retain(|p| p.id != id);
        self.plant(slot, &variety.0);
    }

    /// Total canopy currently standing, a proxy for how well the garden is doing.
    pub fn standing_canopy_cm2(&self) -> f32 {
        self.plants.iter().map(|p| p.canopy_cm2).sum()
    }

    /// Bring the solution straight to full strength.
    ///
    /// A shortcut for tests and for seeding a scenario mid-life. In normal operation
    /// the garden gets here via dosing tasks from the rule engine.
    pub fn feed_to_full_strength(&mut self) {
        let now = self.state.now;
        self.state.tank.set_strength(1.0, now);
    }
}

/// Lets the simulator stand in for real hardware wherever a [`SensorBank`] is expected.
impl SensorBank for Simulation {
    fn capabilities(&self) -> gardyn_core::CapabilitySet {
        self.state.sensors.capabilities()
    }

    fn read(&mut self) -> Result<gardyn_core::SensorSnapshot, HalError> {
        Ok(self.state.sensors.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gardyn_core::{DueWindow, RuleId, Severity};

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn planted_sim() -> Simulation {
        let mut sim = Simulation::new(1, t0());
        sim.plant(SlotId(0), "kale-lacinato");
        sim.plant(SlotId(1), "basil");
        sim
    }

    fn task(kind: TaskKind, target: Target, now: Timestamp) -> Task {
        Task::new(
            kind,
            target,
            Severity::Advisory,
            DueWindow::within_days(now, 1.0),
            "test",
            RuleId::from_static("test"),
        )
    }

    /// Keep a simulation watered and fed, so a test can isolate something other than
    /// drought or starvation.
    fn tick_tended(sim: &mut Simulation, days: u32) {
        for _ in 0..days {
            sim.tick(1.0);
            sim.state.tank.volume_l = sim.state.tank_geometry.capacity_l;
            sim.feed_to_full_strength();
        }
    }

    #[test]
    fn simulation_is_deterministic() {
        let run = |seed| {
            let mut sim = Simulation::new(seed, t0());
            sim.plant(SlotId(0), "kale-lacinato");
            for _ in 0..60 {
                sim.tick(1.0);
            }
            (sim.state.tank.volume_l, sim.standing_canopy_cm2())
        };
        assert_eq!(run(99), run(99));
    }

    #[test]
    fn different_seeds_diverge() {
        let run = |seed| {
            let mut sim = Simulation::new(seed, t0());
            sim.plant(SlotId(0), "kale-lacinato");
            for _ in 0..60 {
                sim.tick(1.0);
            }
            sim.standing_canopy_cm2()
        };
        assert_ne!(run(1), run(2));
    }

    #[test]
    fn seeds_germinate_then_grow() {
        let mut sim = planted_sim();
        for _ in 0..4 {
            sim.tick(1.0);
        }
        // Kale germinates at day 6, so nothing yet.
        assert!(sim.state.plantings[0].germinated_at.is_none());
        assert_eq!(sim.standing_canopy_cm2(), 0.0);

        tick_tended(&mut sim, 20);
        assert!(sim.state.plantings[0].germinated_at.is_some());
        assert!(sim.standing_canopy_cm2() > 10.0);
    }

    #[test]
    fn an_unfed_garden_grows_far_worse_than_a_fed_one() {
        let mut fed = planted_sim();
        tick_tended(&mut fed, 45);

        let mut starved = planted_sim();
        for _ in 0..45 {
            starved.tick(1.0);
            starved.state.tank.volume_l = starved.state.tank_geometry.capacity_l;
        }

        assert!(
            fed.standing_canopy_cm2() > starved.standing_canopy_cm2() * 3.0,
            "fed {:.0} vs starved {:.0}",
            fed.standing_canopy_cm2(),
            starved.standing_canopy_cm2()
        );
    }

    #[test]
    fn a_well_fed_kale_hits_its_harvest_threshold_on_schedule() {
        // Gardyn puts Lacinato Kale at ~14 days to sprout and 58 more to first
        // harvest, with a 380 cm² threshold for its "1 ft" size class. Planted mid
        // column, where the light model peaks. A drift here means the growth model and
        // the published figures have diverged.
        let mut sim = Simulation::new(4, t0());
        sim.plant(SlotId(4), "kale-lacinato");
        tick_tended(&mut sim, 72);
        let canopy = sim.standing_canopy_cm2();
        assert!(
            (250.0..650.0).contains(&canopy),
            "canopy {canopy:.0} cm² is far from the 380 cm² the book expects"
        );
    }

    #[test]
    fn plants_drink_the_tank_down() {
        let mut sim = planted_sim();
        sim.feed_to_full_strength();
        let start = sim.state.tank.volume_l;
        for _ in 0..40 {
            sim.tick(1.0);
        }
        assert!(sim.state.tank.volume_l < start);
        assert!(sim.state.tank.consumption_lpd > 0.0);
    }

    #[test]
    fn a_dry_tank_costs_canopy() {
        let mut sim = planted_sim();
        tick_tended(&mut sim,30);
        let healthy = sim.standing_canopy_cm2();

        sim.state.tank.volume_l = 0.0;
        for _ in 0..14 {
            sim.tick(1.0);
        }
        assert!(sim.standing_canopy_cm2() < healthy, "drought should hurt");
        assert!(sim.dry_days >= 14);
    }

    #[test]
    fn neglected_systems_foul_and_the_pump_shows_it() {
        let mut sim = planted_sim();
        tick_tended(&mut sim,120);
        assert!(sim.fouling_level() > 0.3);
        assert!(
            sim.state.pump.restriction_ratio() > 1.1,
            "ratio was {}",
            sim.state.pump.restriction_ratio()
        );
    }

    #[test]
    fn unfitted_probes_report_nothing() {
        let mut sim = planted_sim();
        sim.tick(1.0);
        // Stock capability set: no EC, no pH, no water temperature.
        assert!(sim.state.sensors.ec_ms_cm.is_none());
        assert!(sim.state.sensors.ph.is_none());
        assert!(sim.state.sensors.water_temp_c.is_none());
        assert!(sim.state.sensors.water_level_mm.is_some());
    }

    #[test]
    fn fitting_a_probe_makes_its_reading_appear() {
        let mut sim = planted_sim();
        sim.enable(Capability::WaterTemperature)
            .enable(Capability::Conductivity);
        sim.tick(1.0);
        assert!(sim.state.sensors.water_temp_c.is_some());
        assert!(sim.state.sensors.ec_ms_cm.is_some());
        assert!(sim.state.sensors.ph.is_none(), "pH probe still not fitted");
    }

    #[test]
    fn vision_stages_gate_their_own_fields() {
        let mut sim = planted_sim();
        tick_tended(&mut sim,20);
        assert!(sim.state.slot_metrics.is_empty(), "vision is off");

        sim.enable(Capability::CanopyMetrics);
        sim.tick(1.0);
        let m = sim.state.slot_metrics.values().next().unwrap();
        assert!(m.canopy_area_cm2 > 0.0);
        assert!(m.plant_count.is_none(), "segmentation is a separate stage");

        sim.enable(Capability::PlantSegmentation);
        sim.tick(1.0);
        let m = sim.state.slot_metrics.values().next().unwrap();
        assert!(m.plant_count.is_some());
    }

    #[test]
    fn the_simulator_satisfies_the_hardware_trait() {
        let mut sim = planted_sim();
        sim.tick(1.0);
        let caps = SensorBank::capabilities(&sim);
        assert!(caps.contains(Capability::WaterLevel));
        assert!(SensorBank::read(&mut sim).is_ok());
    }

    #[test]
    fn harvesting_takes_canopy_and_records_yield() {
        let mut sim = planted_sim();
        tick_tended(&mut sim,60);

        let before = sim.standing_canopy_cm2();
        let id = sim.state.plantings[0].id;
        sim.perform(&task(TaskKind::Harvest, Target::Planting(id), sim.state.now));

        assert!(sim.standing_canopy_cm2() < before);
        assert!(sim.harvested_cm2 > 0.0);
        assert_eq!(sim.state.plantings[0].harvest_count, 1);
    }

    #[test]
    fn a_single_harvest_variety_is_pulled_when_taken() {
        // Wheatgrass is a head crop: Gardyn's article describes one cut, not a cadence.
        let mut sim = Simulation::new(3, t0());
        let id = sim.plant(SlotId(4), "wheatgrass");
        tick_tended(&mut sim, 40);
        sim.perform(&task(TaskKind::Harvest, Target::Planting(id), sim.state.now));
        assert!(sim.state.plantings[0].removed_at.is_some());
        assert_eq!(sim.state.occupied_slots(), 0);
    }

    #[test]
    fn a_deep_clean_resets_fouling_and_the_pump_baseline() {
        let mut sim = planted_sim();
        tick_tended(&mut sim,150);
        assert!(sim.state.pump.restriction_ratio() > 1.1);

        sim.perform(&task(TaskKind::DeepClean, Target::Garden, sim.state.now));

        assert_eq!(sim.fouling_level(), 0.0);
        assert!((sim.state.pump.restriction_ratio() - 1.0).abs() < 0.01);
    }

    #[test]
    fn replanting_frees_and_refills_the_slot() {
        let mut sim = planted_sim();
        tick_tended(&mut sim,10);
        let id = sim.state.plantings[0].id;
        let slot = sim.state.plantings[0].slot;

        sim.perform(&task(TaskKind::Replant, Target::Planting(id), sim.state.now));

        let occupant = sim.state.planting_in(slot).expect("slot should be refilled");
        assert_ne!(occupant.id, id, "a fresh cube, not the old one");
        assert!(occupant.germinated_at.is_none());
    }
}
