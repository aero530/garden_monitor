//! `GardenState`: the immutable snapshot every rule sees.
//!
//! Rules are pure functions of this type. That is what makes them unit-testable, and
//! what makes it possible to replay a season of recorded history against a modified
//! rule to see what it *would* have said.

use crate::capability::CapabilitySet;
use crate::garden::GardenId;
use crate::planting::Planting;
use crate::sensors::{PumpBaseline, SensorSnapshot};
use crate::slot::{Geometry, SlotId};
use crate::tank::{DosingSpec, TankGeometry, TankState};
use crate::variety::{Variety, VarietyBook};
use crate::vision::{AlgaeReading, SlotMetrics};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GardenState {
    /// Which garden this snapshot describes.
    ///
    /// Every piece of state must be attributable to a specific garden once one
    /// account can hold several and share them with others.
    pub garden: GardenId,
    pub now: Timestamp,
    pub geometry: Geometry,
    pub tank_geometry: TankGeometry,
    pub dosing: DosingSpec,
    /// What the garden can currently sense and control. Rules are filtered against this.
    pub capabilities: CapabilitySet,
    pub varieties: VarietyBook,
    pub plantings: Vec<Planting>,
    pub tank: TankState,
    pub sensors: SensorSnapshot,
    pub pump: PumpBaseline,
    /// Present only when a vision stage is enabled.
    pub slot_metrics: BTreeMap<SlotId, SlotMetrics>,
    pub algae: Option<AlgaeReading>,
}

impl GardenState {
    /// A bare Studio 2 with no plantings, stock sensors, and a full tank.
    pub fn new_studio_2(now: Timestamp) -> Self {
        Self::for_garden(GardenId::new(), now)
    }

    /// A bare Studio 2 belonging to a known garden.
    pub fn for_garden(garden: GardenId, now: Timestamp) -> Self {
        let tank_geometry = TankGeometry::STUDIO_2;
        Self {
            garden,
            now,
            geometry: Geometry::STUDIO_2,
            tank_geometry,
            dosing: DosingSpec::default(),
            capabilities: CapabilitySet::stock(),
            varieties: VarietyBook::starter(),
            plantings: Vec::new(),
            tank: TankState::new(tank_geometry.capacity_l),
            sensors: SensorSnapshot::empty(now),
            pump: PumpBaseline::new(Self::NOMINAL_PUMP_MA),
            slot_metrics: BTreeMap::new(),
            algae: None,
        }
    }

    /// Placeholder clean-system pump draw; re-baselined from the real device in Phase 1.
    const NOMINAL_PUMP_MA: f32 = 400.0;

    pub fn active_plantings(&self) -> impl Iterator<Item = &Planting> {
        self.plantings.iter().filter(|p| p.is_active())
    }

    pub fn planting_in(&self, slot: SlotId) -> Option<&Planting> {
        self.active_plantings().find(|p| p.slot == slot)
    }

    pub fn variety_of(&self, planting: &Planting) -> Option<&Variety> {
        self.varieties.get(&planting.variety)
    }

    /// Active plantings paired with their variety. Plantings referencing an unknown
    /// variety are skipped rather than panicking — a stale variety id must never take
    /// the rule engine down.
    pub fn planted(&self) -> impl Iterator<Item = (&Planting, &Variety)> {
        self.active_plantings()
            .filter_map(|p| self.variety_of(p).map(|v| (p, v)))
    }

    pub fn metrics_for(&self, slot: SlotId) -> Option<&SlotMetrics> {
        self.slot_metrics.get(&slot)
    }

    pub fn occupied_slots(&self) -> usize {
        self.active_plantings().count()
    }

    pub fn empty_slots(&self) -> Vec<SlotId> {
        self.geometry
            .slots()
            .filter(|s| self.planting_in(*s).is_none())
            .collect()
    }

    pub fn fill_fraction(&self) -> f32 {
        self.tank.fill_fraction(&self.tank_geometry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planting::PlantingId;
    use crate::variety::VarietyId;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    #[test]
    fn a_fresh_garden_is_empty_and_full() {
        let g = GardenState::new_studio_2(t0());
        assert_eq!(g.occupied_slots(), 0);
        assert_eq!(g.empty_slots().len(), 16);
        assert_eq!(g.fill_fraction(), 1.0);
    }

    #[test]
    fn plantings_resolve_to_varieties() {
        let mut g = GardenState::new_studio_2(t0());
        g.plantings.push(Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new("basil"),
            t0(),
        ));
        let (_, variety) = g.planted().next().unwrap();
        assert_eq!(variety.name, "Basil");
        assert_eq!(g.occupied_slots(), 1);
        assert_eq!(g.empty_slots().len(), 15);
    }

    #[test]
    fn an_unknown_variety_is_skipped_not_fatal() {
        let mut g = GardenState::new_studio_2(t0());
        g.plantings.push(Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new("does-not-exist"),
            t0(),
        ));
        // Still counted as occupying the slot, but contributes no (planting, variety) pair.
        assert_eq!(g.occupied_slots(), 1);
        assert_eq!(g.planted().count(), 0);
    }

    #[test]
    fn removed_plantings_free_their_slot() {
        let mut g = GardenState::new_studio_2(t0());
        let mut p = Planting::new(
            PlantingId(1),
            SlotId(2),
            VarietyId::new("arugula"),
            t0(),
        );
        p.removed_at = Some(t0());
        g.plantings.push(p);
        assert_eq!(g.occupied_slots(), 0);
        assert!(g.empty_slots().contains(&SlotId(2)));
    }
}
