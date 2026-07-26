//! A crude but honest physical model of the garden.
//!
//! The point is not horticultural accuracy — it is to produce a state trajectory
//! realistic enough that the rule engine can be exercised over months in
//! milliseconds, and that a change to a threshold can be evaluated against a whole
//! season rather than a single snapshot. Every coefficient here is a guess to be
//! replaced with fitted values once Phase 1 telemetry exists.

use garden_core::{
    GardenState, PlantingId, SlotId, SlotMetrics, Stage, ewma,
    time::{add_days, days_between},
};

/// Deterministic PRNG.
///
/// Deliberately not `rand`: simulations must be exactly reproducible so that a
/// regression in rule behaviour is distinguishable from a different random draw.
#[derive(Debug, Clone)]
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1))
    }

    pub fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    /// Uniform in `0.0..1.0`.
    pub fn unit(&mut self) -> f32 {
        self.next_u32() as f32 / u32::MAX as f32
    }

    /// Uniform in `-magnitude..=magnitude`.
    pub fn noise(&mut self, magnitude: f32) -> f32 {
        (self.unit() * 2.0 - 1.0) * magnitude
    }

    pub fn chance(&mut self, p: f32) -> bool {
        self.unit() < p
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Environment {
    pub air_temp_c: f32,
    pub humidity_pct: f32,
    /// Hours of light per day.
    pub light_hours: f32,
    /// Bare-surface evaporation, independent of plants.
    pub evaporation_lpd: f32,
    /// Reservoir temperature tends toward air temperature.
    pub water_temp_offset_c: f32,
    /// Sensor noise magnitude.
    pub noise: f32,
}

impl Default for Environment {
    fn default() -> Self {
        Self {
            air_temp_c: 22.0,
            humidity_pct: 45.0,
            light_hours: 15.0,
            evaporation_lpd: 0.08,
            water_temp_offset_c: -1.0,
            noise: 0.02,
        }
    }
}

/// Per-plant simulation state, keyed alongside the domain `Planting`.
#[derive(Debug, Clone)]
pub struct PlantSim {
    pub id: PlantingId,
    pub slot: SlotId,
    pub canopy_cm2: f32,
    /// Seedlings that came up. Germination is patchy in reality.
    pub sprouts: u8,
    pub flowering: bool,
    /// Cumulative harvested canopy, a stand-in for yield.
    pub yielded_cm2: f32,
}

impl PlantSim {
    pub fn new(id: PlantingId, slot: SlotId, sprouts: u8) -> Self {
        Self {
            id,
            slot,
            canopy_cm2: 0.0,
            sprouts,
            flowering: false,
            yielded_cm2: 0.0,
        }
    }
}

/// How solution strength scales growth.
///
/// Starved plants stall; over-strength solution burns roots and also stalls them.
/// The asymmetry matters — the penalty for over-feeding is steeper than for
/// under-feeding, which is precisely the mistake open-loop volume dosing makes.
pub fn nutrient_factor(strength: f32) -> f32 {
    match strength {
        s if s < 0.15 => 0.15,
        s if s < 0.8 => 0.15 + (s - 0.15) / 0.65 * 0.85,
        s if s <= 1.6 => 1.0,
        s if s < 2.6 => 1.0 - (s - 1.6) / 1.0 * 0.7,
        _ => 0.3,
    }
}

/// How reservoir temperature scales growth. Warm water starves roots of oxygen.
pub fn water_temp_factor(temp_c: f32) -> f32 {
    match temp_c {
        t if t < 12.0 => 0.4,
        t if t < 18.0 => 0.4 + (t - 12.0) / 6.0 * 0.6,
        t if t <= 23.0 => 1.0,
        t if t < 29.0 => 1.0 - (t - 23.0) / 6.0 * 0.75,
        _ => 0.25,
    }
}

/// Litres transpired per day by a given canopy area, before environmental scaling.
pub fn transpiration_lpd(canopy_cm2: f32, env: &Environment) -> f32 {
    const L_PER_CM2_PER_DAY: f32 = 0.0016;
    let dryness = ((100.0 - env.humidity_pct) / 55.0).clamp(0.3, 1.6);
    let warmth = (env.air_temp_c / 22.0).clamp(0.5, 1.6);
    let daylight = (env.light_hours / 15.0).clamp(0.3, 1.3);
    canopy_cm2 * L_PER_CM2_PER_DAY * dryness * warmth * daylight
}

/// Conductivity corresponding to a given normalised solution strength.
pub const EC_AT_FULL_STRENGTH: f32 = 1.2;

/// Biofilm growth and its effect on pump draw.
#[derive(Debug, Clone, Copy)]
pub struct Fouling {
    /// 0.0 clean through 1.0 badly fouled.
    pub level: f32,
}

impl Fouling {
    pub const CLEAN: Fouling = Fouling { level: 0.0 };

    /// Fractional increase in pump current at this fouling level.
    pub fn pump_penalty(&self) -> f32 {
        self.level * 0.55
    }

    /// Advance fouling over `days`. Conditioner suppresses the growth rate; warm
    /// water and strong light accelerate it.
    pub fn advance(&mut self, days: f64, days_since_conditioner: f64, env: &Environment) {
        let suppression = if days_since_conditioner <= 7.0 {
            0.25
        } else if days_since_conditioner <= 14.0 {
            0.6
        } else {
            1.0
        };
        let warmth = (env.air_temp_c / 22.0).clamp(0.6, 1.8);
        let rate = 0.010 * suppression * warmth;
        self.level = (self.level + rate * days as f32).clamp(0.0, 1.0);
    }
}

/// Advance every plant's canopy and return total water drawn, in litres.
///
/// Returns `(litres_transpired, nutrient_units_taken)`.
pub fn grow(
    plants: &mut [PlantSim],
    state: &mut GardenState,
    env: &Environment,
    days: f64,
    rng: &mut Lcg,
) -> (f32, f32) {
    let strength = state.tank.estimated_strength();
    let nutrients = nutrient_factor(strength);
    let water_temp = env.air_temp_c + env.water_temp_offset_c;
    let temp = water_temp_factor(water_temp);
    let dry = state.tank.volume_l <= 0.05;

    let geometry = state.geometry;
    let now = state.now;

    let mut transpired = 0.0;
    let mut uptake = 0.0;

    for plant in plants.iter_mut() {
        let Some((planting, variety)) = state
            .plantings
            .iter()
            .find(|p| p.id == plant.id && p.is_active())
            .and_then(|p| state.varieties.get(&p.variety).map(|v| (p, v)))
        else {
            continue;
        };

        let stage = planting.stage(variety, now);
        if matches!(stage, Stage::Seeded) {
            continue;
        }

        // A dry tank costs canopy rather than merely pausing growth.
        if dry {
            plant.canopy_cm2 *= 1.0 - (0.06 * days as f32).min(0.5);
            continue;
        }

        let light = geometry.light_exposure(plant.slot);

        // Logistic growth toward a ceiling set by the variety and the slot's light.
        let ceiling = variety.harvest_canopy_cm2.unwrap_or(300.0) * 1.7 * light;
        let vigour = if matches!(stage, Stage::Declining) {
            0.35
        } else {
            1.0
        };
        // Calibrated so a well-fed kale in a bright slot reaches its 380 cm² harvest
        // threshold around 58 days after germination, matching Gardyn's figures.
        let rate = 0.17 * nutrients * temp * light * vigour * (1.0 + rng.noise(env.noise));
        let seeded = plant.canopy_cm2.max(2.0);
        let delta = rate * seeded * (1.0 - seeded / ceiling) * days as f32;
        plant.canopy_cm2 = (plant.canopy_cm2 + delta).clamp(0.0, ceiling);

        if variety.needs_pollination && stage.is_producing() {
            plant.flowering = true;
        }

        let t = transpiration_lpd(plant.canopy_cm2, env) * days as f32;
        transpired += t;
        // Plants take up nutrient roughly in proportion to water, but less than
        // proportionally — which is why solution strength drifts upward over time.
        uptake += t * strength * 0.75;
    }

    (transpired, uptake)
}

/// Refresh the derived sensor snapshot from ground truth, adding measurement noise.
pub fn sense(state: &mut GardenState, env: &Environment, fouling: Fouling, rng: &mut Lcg) {
    let n = env.noise;
    let geometry = state.tank_geometry;

    state.sensors.at = state.now;
    state.sensors.air_temp_c = Some(env.air_temp_c * (1.0 + rng.noise(n)));
    state.sensors.humidity_pct = Some(env.humidity_pct * (1.0 + rng.noise(n)));
    state.sensors.pcb_temp_c = Some(env.air_temp_c + 8.0);

    // Ultrasonic distance is the inverse of the volume mapping.
    let fill = geometry.fill_fraction(state.tank.volume_l);
    let distance =
        geometry.empty_distance_mm - fill * (geometry.empty_distance_mm - geometry.full_distance_mm);
    state.sensors.water_level_mm = Some(distance * (1.0 + rng.noise(n)));

    state.sensors.water_temp_c = Some((env.air_temp_c + env.water_temp_offset_c) * (1.0 + rng.noise(n * 0.5)));

    let draw = state.pump.nominal_ma * (1.0 + fouling.pump_penalty()) * (1.0 + rng.noise(n));
    state.sensors.pump_current_ma = Some(draw);
    state.pump.current_ma_ewma = ewma(state.pump.current_ma_ewma, draw, 0.25);

    state.sensors.ec_ms_cm = Some(state.tank.estimated_strength() * EC_AT_FULL_STRENGTH);
    // pH drifts alkaline as nutrients are consumed.
    let drift = (1.0 - state.tank.estimated_strength().min(1.5)) * 0.6;
    state.sensors.ph = Some((5.9 + drift).clamp(4.5, 8.0));
}

/// Publish per-slot vision metrics from ground truth.
///
/// The real pipeline undistorts a frame and measures pixels; here we hand the rules
/// the true canopy area plus noise. That is the right level of fidelity for testing
/// rule behaviour — the accuracy of the measurement itself is `garden-vision`'s
/// problem, not the rule engine's.
pub fn observe(state: &mut GardenState, plants: &[PlantSim], rng: &mut Lcg, env: &Environment) {
    state.slot_metrics.clear();
    for plant in plants {
        let Some(planting) = state
            .plantings
            .iter()
            .find(|p| p.id == plant.id && p.is_active())
        else {
            continue;
        };

        let area = plant.canopy_cm2 * (1.0 + rng.noise(env.noise * 2.0));
        let mut metrics = SlotMetrics::new(plant.slot, state.now, area.max(0.0));
        metrics.green_fraction = (area / 900.0).clamp(0.0, 1.0);
        metrics.plant_count = Some(plant.sprouts);
        metrics.flowering = Some(plant.flowering);

        // Chlorosis tracks starvation.
        let strength = state.tank.estimated_strength();
        metrics.yellowing_index = if strength < 0.4 {
            ((0.4 - strength) / 0.4).clamp(0.0, 1.0) * 0.8
        } else {
            0.05
        };

        let age = planting
            .days_since_germination(state.now)
            .unwrap_or(0.0)
            .max(1.0);
        metrics.growth_rate_cm2_per_day = (area / age as f32).max(0.0);

        state.slot_metrics.insert(plant.slot, metrics);
    }
}

/// Mark plantings as germinated once they have been in long enough.
pub fn germinate(state: &mut GardenState, plants: &mut [PlantSim], rng: &mut Lcg) {
    let now = state.now;
    let mut sprouted = Vec::new();

    for planting in state.plantings.iter().filter(|p| p.is_active()) {
        if planting.germinated_at.is_some() {
            continue;
        }
        let Some(variety) = state.varieties.get(&planting.variety) else {
            continue;
        };
        if days_between(planting.planted_at, now) >= f64::from(variety.germination_days) {
            sprouted.push(planting.id);
        }
    }

    for id in sprouted {
        if let Some(p) = state.plantings.iter_mut().find(|p| p.id == id) {
            p.germinated_at = Some(now);
        }
        if let Some(plant) = plants.iter_mut().find(|p| p.id == id) {
            plant.canopy_cm2 = 3.0;
            // Germination is patchy: somewhere between two and six come up.
            plant.sprouts = 2 + (rng.next_u32() % 5) as u8;
        }
    }
}

/// Update the tank's consumption estimate from the observed volume change.
pub fn update_consumption(state: &mut GardenState, previous_volume_l: f32, days: f64) {
    if days <= 0.0 {
        return;
    }
    let drop = previous_volume_l - state.tank.volume_l;
    // Ignore the step change caused by a refill.
    if drop <= 0.0 {
        return;
    }
    let observed = drop / days as f32;
    state.tank.consumption_lpd = if state.tank.consumption_lpd <= 0.0 {
        observed
    } else {
        ewma(state.tank.consumption_lpd, observed, 0.3)
    };
}

/// Advance the clock on the state.
pub fn advance_clock(state: &mut GardenState, days: f64) {
    state.now = add_days(state.now, days);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prng_is_reproducible() {
        let a: Vec<u32> = (0..5).map(|_| Lcg::new(42).next_u32()).collect();
        let b: Vec<u32> = (0..5).map(|_| Lcg::new(42).next_u32()).collect();
        assert_eq!(a, b);

        let mut seq1 = Lcg::new(7);
        let mut seq2 = Lcg::new(7);
        for _ in 0..100 {
            assert_eq!(seq1.next_u32(), seq2.next_u32());
        }
    }

    #[test]
    fn prng_unit_stays_in_range() {
        let mut rng = Lcg::new(1);
        for _ in 0..1000 {
            let u = rng.unit();
            assert!((0.0..=1.0).contains(&u), "{u} out of range");
        }
    }

    #[test]
    fn over_feeding_is_punished_more_steeply_than_under_feeding() {
        // The asymmetry that makes open-loop volume dosing risky.
        let starved = nutrient_factor(0.3);
        let ideal = nutrient_factor(1.0);
        let burnt = nutrient_factor(3.0);
        assert!(ideal > starved && ideal > burnt);
        assert!(burnt < starved, "over-strength should be worse than lean");
    }

    #[test]
    fn warm_water_suppresses_growth() {
        assert!((water_temp_factor(21.0) - 1.0).abs() < f32::EPSILON);
        assert!(water_temp_factor(28.0) < 0.5);
        assert!(water_temp_factor(10.0) < 0.5);
    }

    #[test]
    fn bigger_canopy_drinks_more() {
        let env = Environment::default();
        assert!(transpiration_lpd(600.0, &env) > transpiration_lpd(200.0, &env));
    }

    #[test]
    fn dry_air_drives_more_transpiration_than_humid() {
        let dry = Environment {
            humidity_pct: 20.0,
            ..Default::default()
        };
        let humid = Environment {
            humidity_pct: 80.0,
            ..Default::default()
        };
        assert!(transpiration_lpd(400.0, &dry) > transpiration_lpd(400.0, &humid));
    }

    #[test]
    fn conditioner_slows_fouling() {
        let env = Environment::default();
        let mut treated = Fouling::CLEAN;
        let mut neglected = Fouling::CLEAN;
        for _ in 0..60 {
            treated.advance(1.0, 3.0, &env);
            neglected.advance(1.0, 30.0, &env);
        }
        assert!(neglected.level > treated.level * 2.0);
    }

    #[test]
    fn fouling_raises_pump_draw() {
        assert_eq!(Fouling::CLEAN.pump_penalty(), 0.0);
        assert!(Fouling { level: 0.8 }.pump_penalty() > 0.4);
    }

    #[test]
    fn consumption_estimate_ignores_refills() {
        use garden_core::Timestamp;
        let mut state = GardenState::new_studio_2(Timestamp::from_second(0).unwrap());
        state.tank.volume_l = 10.0;
        // Volume went up, not down — a refill, not consumption.
        update_consumption(&mut state, 4.0, 1.0);
        assert_eq!(state.tank.consumption_lpd, 0.0);

        state.tank.volume_l = 9.0;
        update_consumption(&mut state, 10.0, 1.0);
        assert_eq!(state.tank.consumption_lpd, 1.0);
    }
}
