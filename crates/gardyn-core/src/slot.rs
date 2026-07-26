//! Slot addressing and tower geometry.
//!
//! Position is not cosmetic. In a vertical tower the top rows sit closest to the
//! light bar and receive water first, so identical varieties in different rows grow
//! at measurably different rates. Rules and the succession planner both need to know
//! where a planting physically sits.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Zero-based slot index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SlotId(pub u8);

impl fmt::Display for SlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Operator-facing numbering is 1-based; nobody labels a physical slot "0".
        write!(f, "slot {}", self.0 + 1)
    }
}

/// Physical arrangement of the tower.
///
/// The Studio 2 holds 16 plants, but the column/row split is **unconfirmed** — it is
/// one of the items on the Phase 0 recon list. Keeping it configurable means the
/// discovery does not invalidate any code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Geometry {
    pub columns: u8,
    pub rows_per_column: u8,
}

impl Geometry {
    /// Working assumption for the Studio 2: two columns of eight. Verify in Phase 0.
    pub const STUDIO_2: Geometry = Geometry {
        columns: 2,
        rows_per_column: 8,
    };

    pub fn slot_count(&self) -> u8 {
        self.columns.saturating_mul(self.rows_per_column)
    }

    pub fn contains(&self, slot: SlotId) -> bool {
        slot.0 < self.slot_count()
    }

    /// Slots are numbered column-major: column 0 top to bottom, then column 1.
    pub fn position(&self, slot: SlotId) -> Option<SlotPosition> {
        if !self.contains(slot) || self.rows_per_column == 0 {
            return None;
        }
        Some(SlotPosition {
            column: slot.0 / self.rows_per_column,
            row: slot.0 % self.rows_per_column,
        })
    }

    pub fn slots(&self) -> impl Iterator<Item = SlotId> {
        (0..self.slot_count()).map(SlotId)
    }
}

/// Row 0 is the top of the tower.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotPosition {
    pub column: u8,
    pub row: u8,
}

impl SlotPosition {
    /// Relative light exposure in `0.0..=1.0`, 1.0 at the top row.
    ///
    /// A linear falloff is a placeholder. Once `CanopyMetrics` has a season of data,
    /// the real per-row curve can be fitted from observed growth rates and this
    /// becomes a lookup instead of a guess.
    pub fn light_exposure(&self, geometry: &Geometry) -> f32 {
        if geometry.rows_per_column <= 1 {
            return 1.0;
        }
        let last_row = f32::from(geometry.rows_per_column - 1);
        1.0 - (f32::from(self.row) / last_row) * Self::LIGHT_FALLOFF
    }

    /// Fraction of top-row light reaching the bottom row.
    const LIGHT_FALLOFF: f32 = 0.35;

    /// Whether this position favours large, light-hungry varieties.
    pub fn suits_large_canopy(&self, geometry: &Geometry) -> bool {
        self.light_exposure(geometry) >= 0.85
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn studio_2_has_sixteen_slots() {
        assert_eq!(Geometry::STUDIO_2.slot_count(), 16);
        assert_eq!(Geometry::STUDIO_2.slots().count(), 16);
    }

    #[test]
    fn slots_are_column_major() {
        let g = Geometry::STUDIO_2;
        assert_eq!(
            g.position(SlotId(0)),
            Some(SlotPosition { column: 0, row: 0 })
        );
        assert_eq!(
            g.position(SlotId(7)),
            Some(SlotPosition { column: 0, row: 7 })
        );
        assert_eq!(
            g.position(SlotId(8)),
            Some(SlotPosition { column: 1, row: 0 })
        );
    }

    #[test]
    fn out_of_range_slot_has_no_position() {
        assert_eq!(Geometry::STUDIO_2.position(SlotId(16)), None);
    }

    #[test]
    fn top_row_gets_the_most_light() {
        let g = Geometry::STUDIO_2;
        let top = g.position(SlotId(0)).unwrap();
        let bottom = g.position(SlotId(7)).unwrap();
        assert_eq!(top.light_exposure(&g), 1.0);
        assert!(bottom.light_exposure(&g) < top.light_exposure(&g));
        assert!(top.suits_large_canopy(&g));
        assert!(!bottom.suits_large_canopy(&g));
    }

    #[test]
    fn slot_display_is_one_based_for_humans() {
        assert_eq!(SlotId(0).to_string(), "slot 1");
    }
}
