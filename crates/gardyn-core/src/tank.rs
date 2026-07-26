//! Reservoir state: volume, consumption, and the nutrient mass balance.

use crate::task::TaskKind;
use crate::time::{add_days, days_since_or_never};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// Maps ultrasonic distance readings to volume.
///
/// The DYP-A01 measures from the sensor face down to the water surface, so distance
/// is *inversely* proportional to volume. The two calibration distances are recorded
/// once, by filling the tank and draining it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TankGeometry {
    pub capacity_l: f32,
    /// Distance reading when full.
    pub full_distance_mm: f32,
    /// Distance reading when empty.
    pub empty_distance_mm: f32,
}

impl TankGeometry {
    /// Studio 2 ships a "4+ gallon" tank; 15.5 L is a conservative usable figure.
    /// Calibration distances are placeholders until Phase 0 measures the real ones.
    pub const STUDIO_2: TankGeometry = TankGeometry {
        capacity_l: 15.5,
        full_distance_mm: 60.0,
        empty_distance_mm: 330.0,
    };

    /// Convert a distance reading to litres, clamped to the physical range.
    pub fn volume_from_distance(&self, distance_mm: f32) -> f32 {
        let span = self.empty_distance_mm - self.full_distance_mm;
        if span.abs() < f32::EPSILON {
            return 0.0;
        }
        let filled = (self.empty_distance_mm - distance_mm) / span;
        (filled.clamp(0.0, 1.0)) * self.capacity_l
    }

    pub fn fill_fraction(&self, volume_l: f32) -> f32 {
        if self.capacity_l <= 0.0 {
            return 0.0;
        }
        (volume_l / self.capacity_l).clamp(0.0, 1.0)
    }
}

/// How much plant food and conditioner to add per litre of water.
///
/// **Placeholder values.** Confirm against the label on the bottle before these drive
/// a real dose instruction; they are structured as configuration precisely so that
/// correcting them is a data change, not a code change.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DosingSpec {
    pub food_ml_per_litre: f32,
    pub conditioner_ml_per_litre: f32,
    /// Germinating seeds are burned by full-strength solution, so the first weeks run
    /// at a reduced rate.
    pub sprout_dose_fraction: f32,
    /// Conductivity added per mL of food per litre of water, in mS/cm.
    ///
    /// Unused until an EC probe is fitted, and calibrated by measurement at that
    /// point: dose a known volume, read the delta. Until then it is only a plausible
    /// starting value.
    pub ec_per_ml_per_litre: f32,
}

impl Default for DosingSpec {
    fn default() -> Self {
        Self {
            food_ml_per_litre: 2.0,
            conditioner_ml_per_litre: 1.0,
            sprout_dose_fraction: 0.5,
            ec_per_ml_per_litre: 0.35,
        }
    }
}

impl DosingSpec {
    /// Millilitres of food needed to raise `volume_l` by `delta_ec` mS/cm.
    pub fn food_ml_for_ec_delta(&self, delta_ec: f32, volume_l: f32) -> f32 {
        if self.ec_per_ml_per_litre <= 0.0 || volume_l <= 0.0 {
            return 0.0;
        }
        (delta_ec / self.ec_per_ml_per_litre) * volume_l
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TankState {
    pub volume_l: f32,
    /// Exponentially weighted mean daily consumption. This is aggregate plant
    /// transpiration plus evaporation, and doubles as a whole-garden health proxy: a
    /// sharp fall means something is wrong before any single plant looks wrong.
    pub consumption_lpd: f32,
    pub last_top_off: Option<Timestamp>,
    pub last_refresh: Option<Timestamp>,
    pub last_food_dose: Option<Timestamp>,
    pub last_conditioner: Option<Timestamp>,
    pub last_deep_clean: Option<Timestamp>,
    /// Water added since the last food dose. Drives the fallback dosing rule when no
    /// EC probe is fitted.
    pub litres_added_since_food_dose: f32,
    /// Mass-balance estimate of dissolved nutrient, in normalised dose-units where
    /// 1.0 unit per litre is full strength. Only an estimate; an EC probe supersedes it.
    pub nutrient_units: f32,
}

impl TankState {
    pub fn new(volume_l: f32) -> Self {
        Self {
            volume_l,
            consumption_lpd: 0.0,
            last_top_off: None,
            last_refresh: None,
            last_food_dose: None,
            last_conditioner: None,
            last_deep_clean: None,
            // The initial fill is plain water that no dose has accounted for. Starting
            // this at zero would deadlock a new garden: dosing is triggered by water
            // added since the last dose, top-offs only happen once plants drink the
            // tank down, and plants only grow once they have been fed.
            litres_added_since_food_dose: volume_l,
            nutrient_units: 0.0,
        }
    }

    pub fn fill_fraction(&self, geometry: &TankGeometry) -> f32 {
        geometry.fill_fraction(self.volume_l)
    }

    /// Days until the tank reaches `threshold_l` at the current consumption rate.
    /// `None` when consumption is negligible, which would otherwise divide by zero.
    pub fn days_until(&self, threshold_l: f32) -> Option<f64> {
        if self.consumption_lpd <= Self::MIN_MEANINGFUL_CONSUMPTION {
            return None;
        }
        let headroom = self.volume_l - threshold_l;
        Some(f64::from(headroom / self.consumption_lpd))
    }

    pub fn days_until_empty(&self) -> Option<f64> {
        self.days_until(0.0)
    }

    /// When the tank will reach `threshold_l`, for scheduling a due window.
    pub fn projected_time_at(&self, threshold_l: f32, now: Timestamp) -> Option<Timestamp> {
        self.days_until(threshold_l).map(|d| add_days(now, d))
    }

    /// Below this the rate is noise, not signal.
    const MIN_MEANINGFUL_CONSUMPTION: f32 = 0.01;

    /// Estimated solution strength as a fraction of full. 1.0 is on target.
    pub fn estimated_strength(&self) -> f32 {
        if self.volume_l <= 0.0 {
            return 0.0;
        }
        self.nutrient_units / self.volume_l
    }

    pub fn days_since_refresh(&self, now: Timestamp) -> f64 {
        days_since_or_never(self.last_refresh, now)
    }

    pub fn days_since_conditioner(&self, now: Timestamp) -> f64 {
        days_since_or_never(self.last_conditioner, now)
    }

    pub fn days_since_deep_clean(&self, now: Timestamp) -> f64 {
        days_since_or_never(self.last_deep_clean, now)
    }

    pub fn days_since_food_dose(&self, now: Timestamp) -> f64 {
        days_since_or_never(self.last_food_dose, now)
    }

    // --- Mutations, applied when the operator confirms an action ----------------

    /// Record a top-off. Nutrient mass is unchanged, so the solution dilutes.
    pub fn top_off(&mut self, litres: f32, geometry: &TankGeometry, at: Timestamp) {
        self.volume_l = (self.volume_l + litres).min(geometry.capacity_l);
        self.litres_added_since_food_dose += litres;
        self.last_top_off = Some(at);
    }

    /// Record a food dose. `units` is in the same normalised scale as `nutrient_units`.
    pub fn add_food(&mut self, units: f32, at: Timestamp) {
        self.nutrient_units += units;
        self.litres_added_since_food_dose = 0.0;
        self.last_food_dose = Some(at);
    }

    /// Set the solution to an exact strength, rather than adding to it.
    ///
    /// Distinct from [`TankState::add_food`] because repeatedly *adding* a full dose
    /// compounds into an over-strength solution — the same mistake open-loop dosing
    /// makes in practice, and one worth keeping impossible to make by accident.
    pub fn set_strength(&mut self, strength: f32, at: Timestamp) {
        self.nutrient_units = self.volume_l * strength.max(0.0);
        self.litres_added_since_food_dose = 0.0;
        self.last_food_dose = Some(at);
    }

    pub fn add_conditioner(&mut self, at: Timestamp) {
        self.last_conditioner = Some(at);
    }

    /// A tank refresh empties and refills, resetting the nutrient balance entirely.
    pub fn refresh(&mut self, fill_to_l: f32, geometry: &TankGeometry, at: Timestamp) {
        self.volume_l = fill_to_l.min(geometry.capacity_l);
        self.nutrient_units = 0.0;
        self.litres_added_since_food_dose = self.volume_l;
        self.last_refresh = Some(at);
        self.last_top_off = Some(at);
    }

    pub fn deep_clean(&mut self, at: Timestamp) {
        self.last_deep_clean = Some(at);
    }

    /// Water leaving the tank. Transpiration and evaporation remove water but leave
    /// dissolved salts behind, which is why strength drifts upward between top-offs.
    pub fn consume_water(&mut self, litres: f32) {
        self.volume_l = (self.volume_l - litres).max(0.0);
    }

    /// Nutrient taken up by the plants, removing mass from solution.
    pub fn consume_nutrient(&mut self, units: f32) {
        self.nutrient_units = (self.nutrient_units - units).max(0.0);
    }
}

/// Something the operator did to the tank.
///
/// The rule engine is stateless: it re-derives "you are overdue for a tank refresh"
/// from `last_refresh` every time it runs. So an action that is not recorded did not
/// happen, and the task comes back on the next tick looking exactly as it did before.
///
/// This is the tank's counterpart to a planting event, and it exists for the same
/// reason: completion has to write back to the thing the rule reads, or marking a task
/// done silently undoes itself.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TankEvent {
    /// Water added. Dilutes the solution, since nutrient mass is unchanged.
    TopOff { litres: f32 },
    /// Emptied and refilled. Resets the nutrient balance entirely.
    Refresh { fill_to_l: f32 },
    /// Plant food added, in normalised dose-units. Adds to what is already there.
    FoodDose { units: f32 },
    /// Solution brought *to* a strength, rather than having a dose added to it.
    ///
    /// This is what completing a "feed the tank" task means, and it is deliberately
    /// not `FoodDose`. Adding a full dose to a tank that is already half-strength
    /// compounds, which is exactly the mistake open-loop dosing makes in practice.
    FedToStrength { strength: f32 },
    /// Water conditioner, HydroBoost or equivalent.
    Conditioner,
    /// Full strip-down and scrub.
    DeepClean,
}

impl TankEvent {
    pub fn apply(self, tank: &mut TankState, geometry: &TankGeometry, at: Timestamp) {
        match self {
            TankEvent::TopOff { litres } => tank.top_off(litres, geometry, at),
            TankEvent::Refresh { fill_to_l } => tank.refresh(fill_to_l, geometry, at),
            TankEvent::FoodDose { units } => tank.add_food(units, at),
            TankEvent::FedToStrength { strength } => tank.set_strength(strength, at),
            TankEvent::Conditioner => tank.add_conditioner(at),
            TankEvent::DeepClean => tank.deep_clean(at),
        }
    }

    /// Which event completing a task of this kind represents, if any.
    ///
    /// `AddWater` deliberately maps to nothing. How much went in is not knowable from
    /// a button press, and the level sensor measures it directly — inventing a litre
    /// count here would corrupt the mass balance the dosing rule depends on.
    pub fn for_task(kind: TaskKind, geometry: &TankGeometry) -> Option<Self> {
        match kind {
            TaskKind::AddPlantFood => Some(TankEvent::FedToStrength { strength: 1.0 }),
            TaskKind::AddConditioner => Some(TankEvent::Conditioner),
            TaskKind::TankRefresh => Some(TankEvent::Refresh {
                fill_to_l: geometry.capacity_l,
            }),
            TaskKind::DeepClean => Some(TankEvent::DeepClean),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            TankEvent::TopOff { .. } => "topped off",
            TankEvent::Refresh { .. } => "refreshed",
            TankEvent::FoodDose { .. } | TankEvent::FedToStrength { .. } => "fed",
            TankEvent::Conditioner => "conditioned",
            TankEvent::DeepClean => "deep cleaned",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    #[test]
    fn distance_maps_inversely_to_volume() {
        let g = TankGeometry::STUDIO_2;
        assert_eq!(g.volume_from_distance(g.full_distance_mm), g.capacity_l);
        assert_eq!(g.volume_from_distance(g.empty_distance_mm), 0.0);
        let mid = (g.full_distance_mm + g.empty_distance_mm) / 2.0;
        assert!((g.volume_from_distance(mid) - g.capacity_l / 2.0).abs() < 0.01);
    }

    #[test]
    fn out_of_range_distances_clamp() {
        let g = TankGeometry::STUDIO_2;
        assert_eq!(g.volume_from_distance(0.0), g.capacity_l);
        assert_eq!(g.volume_from_distance(9999.0), 0.0);
    }

    #[test]
    fn a_fresh_tank_already_owes_a_dose() {
        // Otherwise a new garden deadlocks: never dosed, never topped off, never fed.
        let tank = TankState::new(15.5);
        assert_eq!(tank.litres_added_since_food_dose, 15.5);
        assert_eq!(tank.nutrient_units, 0.0);
    }

    #[test]
    fn forecast_needs_a_meaningful_consumption_rate() {
        let mut tank = TankState::new(10.0);
        assert_eq!(tank.days_until_empty(), None);
        tank.consumption_lpd = 0.5;
        assert_eq!(tank.days_until_empty(), Some(20.0));
        assert_eq!(tank.days_until(6.0), Some(8.0));
    }

    #[test]
    fn topping_off_dilutes_the_solution() {
        let g = TankGeometry::STUDIO_2;
        let mut tank = TankState::new(10.0);
        tank.add_food(10.0, t0());
        assert_eq!(tank.estimated_strength(), 1.0);
        tank.top_off(5.0, &g, t0());
        assert!((tank.estimated_strength() - 10.0 / 15.0).abs() < 1e-6);
        assert_eq!(tank.litres_added_since_food_dose, 5.0);
    }

    #[test]
    fn evaporation_concentrates_the_solution() {
        let mut tank = TankState::new(10.0);
        tank.add_food(10.0, t0());
        tank.consume_water(5.0);
        // Same salts, half the water.
        assert_eq!(tank.estimated_strength(), 2.0);
    }

    #[test]
    fn refresh_resets_the_nutrient_balance() {
        let g = TankGeometry::STUDIO_2;
        let mut tank = TankState::new(4.0);
        tank.add_food(20.0, t0());
        tank.refresh(12.0, &g, t0());
        assert_eq!(tank.nutrient_units, 0.0);
        assert_eq!(tank.volume_l, 12.0);
        assert_eq!(tank.estimated_strength(), 0.0);
    }

    #[test]
    fn top_off_cannot_overfill() {
        let g = TankGeometry::STUDIO_2;
        let mut tank = TankState::new(15.0);
        tank.top_off(100.0, &g, t0());
        assert_eq!(tank.volume_l, g.capacity_l);
    }

    #[test]
    fn consumption_cannot_drive_volume_negative() {
        let mut tank = TankState::new(1.0);
        tank.consume_water(5.0);
        assert_eq!(tank.volume_l, 0.0);
        assert_eq!(tank.estimated_strength(), 0.0);
    }
}
