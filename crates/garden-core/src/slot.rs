//! Slot addressing, tower geometry, and light zones.
//!
//! Position is not cosmetic. Gardyn's own placement guide divides the tower into
//! high, medium and low light zones and tells you to put fruiting plants "in the
//! center of your Gardyn, where the light reaches maximum intensity". Which slot a
//! cube goes in therefore decides whether it thrives, and the rules need to know.

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

/// How much light a position receives, as Gardyn categorises it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LightZone {
    /// Bottom of the range: mints, cilantro, most soft herbs.
    Low,
    /// Greens and leafy herbs.
    Medium,
    /// Fruiting plants and flowers. The brightest part of the tower.
    High,
}

impl LightZone {
    pub const ALL: &'static [LightZone] = &[LightZone::Low, LightZone::Medium, LightZone::High];

    pub fn label(self) -> &'static str {
        match self {
            LightZone::Low => "low light",
            LightZone::Medium => "medium light",
            LightZone::High => "high light",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            LightZone::Low => "low",
            LightZone::Medium => "medium",
            LightZone::High => "high",
        }
    }

    /// Whether a plant wanting `self` is adequately served by a slot in `available`.
    ///
    /// Asymmetric on purpose. A high-light plant in a dim slot sulks and never fruits,
    /// which is a real problem; a low-light herb in a bright slot merely grows fast.
    pub fn satisfied_by(self, available: LightZone) -> bool {
        available >= self
    }
}

impl fmt::Display for LightZone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Physical arrangement of the tower.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Geometry {
    pub columns: u8,
    pub rows_per_column: u8,
}

impl Geometry {
    /// Studio and Studio 2: two columns of eight.
    pub const STUDIO_2: Geometry = Geometry {
        columns: 2,
        rows_per_column: 8,
    };

    /// Home 3 and Home 4: three columns of ten, per Gardyn's setup documentation.
    pub const HOME: Geometry = Geometry {
        columns: 3,
        rows_per_column: 10,
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

    /// The slot at a given column and row, if it exists.
    pub fn slot_at(&self, column: u8, row: u8) -> Option<SlotId> {
        (column < self.columns && row < self.rows_per_column)
            .then(|| SlotId(column * self.rows_per_column + row))
    }

    pub fn slots(&self) -> impl Iterator<Item = SlotId> {
        (0..self.slot_count()).map(SlotId)
    }

    /// Gardyn's published per-slot light zones, where they exist.
    ///
    /// Indexed by slot id, column-major. Transcribed from the welcome-kit placement
    /// cards, which colour every position by light intensity. Note the pattern is
    /// **staggered, not a smooth gradient** — the two columns put their high-light
    /// slots on alternating rows, which is what you would expect when yPods sit around
    /// the column rather than facing the light square-on. No model I would have
    /// written produces that, which is why the published map wins.
    pub fn zone_map(&self) -> Option<&'static [LightZone]> {
        (*self == Self::STUDIO_2).then_some(STUDIO_2_ZONES)
    }

    /// Which of Gardyn's three planting zones a slot falls into.
    pub fn light_zone(&self, slot: SlotId) -> LightZone {
        if let Some(map) = self.zone_map()
            && let Some(zone) = map.get(usize::from(slot.0))
        {
            return *zone;
        }
        self.position(slot)
            .map(|p| p.light_zone(self))
            .unwrap_or(LightZone::Medium)
    }

    /// Relative light intensity at a slot, in `0.0..=1.0`.
    ///
    /// Where Gardyn publishes a zone map this is derived from it, so the growth model
    /// and the placement advice cannot disagree about which slots are bright.
    pub fn light_exposure(&self, slot: SlotId) -> f32 {
        if self.zone_map().is_some() {
            return match self.light_zone(slot) {
                LightZone::High => 1.0,
                LightZone::Medium => 0.85,
                LightZone::Low => 0.68,
            };
        }
        self.position(slot)
            .map(|p| p.light_exposure(self))
            .unwrap_or(1.0)
    }

    /// Slots of one column, top to bottom. This is the order the UI renders.
    pub fn column(&self, column: u8) -> impl Iterator<Item = SlotId> + '_ {
        (0..self.rows_per_column).filter_map(move |row| self.slot_at(column, row))
    }
}

/// Studio 2 light zones by slot, from Gardyn's welcome-kit placement cards.
///
/// Left column then right column, each top to bottom. Five high, nine medium, two low.
const STUDIO_2_ZONES: &[LightZone] = &[
    // Left column.
    LightZone::Medium,
    LightZone::Medium,
    LightZone::High,
    LightZone::Medium,
    LightZone::High,
    LightZone::Medium,
    LightZone::High,
    LightZone::Low,
    // Right column.
    LightZone::Low,
    LightZone::Medium,
    LightZone::Medium,
    LightZone::High,
    LightZone::Medium,
    LightZone::High,
    LightZone::Medium,
    LightZone::Medium,
];

/// Row 0 is the top of the tower.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotPosition {
    pub column: u8,
    pub row: u8,
}

impl SlotPosition {
    /// Relative light intensity in `0.0..=1.0`.
    ///
    /// Peaks at the vertical middle of the tower, not the top. The light bars run the
    /// full height beside the columns, so the middle of a column sees emitters above
    /// and below it while the end slots see only one side — which is why Gardyn's
    /// guide puts fruiting plants "in the center".
    ///
    /// A calibration target, not a measurement. Once `CanopyMetrics` has a season of
    /// data the real per-position curve can be fitted from observed growth rates and
    /// this becomes a lookup.
    pub fn light_exposure(&self, geometry: &Geometry) -> f32 {
        self.vertical_factor(geometry) * self.horizontal_factor(geometry)
    }

    /// Distance from the vertical centre of the column, as a falloff.
    fn vertical_factor(&self, geometry: &Geometry) -> f32 {
        if geometry.rows_per_column <= 1 {
            return 1.0;
        }
        let last = f32::from(geometry.rows_per_column - 1);
        let centre = last / 2.0;
        let distance = (f32::from(self.row) - centre).abs() / centre;
        1.0 - distance * Self::VERTICAL_FALLOFF
    }

    /// Middle columns are flanked by light on both sides; outer columns are not.
    fn horizontal_factor(&self, geometry: &Geometry) -> f32 {
        if geometry.columns <= 2 {
            // With one or two columns every column is an outer column, and the light
            // bars sit around the whole tower, so there is nothing to distinguish.
            return 1.0;
        }
        let last = f32::from(geometry.columns - 1);
        let centre = last / 2.0;
        let distance = (f32::from(self.column) - centre).abs() / centre;
        1.0 - distance * Self::HORIZONTAL_FALLOFF
    }

    /// Fraction of centre-row light lost at the very top or bottom of a column.
    const VERTICAL_FALLOFF: f32 = 0.30;
    /// Fraction lost at an outer column relative to the middle one.
    const HORIZONTAL_FALLOFF: f32 = 0.18;

    /// Which of Gardyn's three planting zones this position falls into.
    pub fn light_zone(&self, geometry: &Geometry) -> LightZone {
        match self.light_exposure(geometry) {
            e if e >= Self::HIGH_ZONE_THRESHOLD => LightZone::High,
            e if e >= Self::MEDIUM_ZONE_THRESHOLD => LightZone::Medium,
            _ => LightZone::Low,
        }
    }

    /// Calibrated so a Studio 2 comes out as 4 high, 8 medium and 4 low slots.
    ///
    /// That split matters: Gardyn's catalogue is dominated by medium-light greens, and
    /// a tower that reported most of its slots as low-light would have the placement
    /// advice objecting to a perfectly ordinary salad planting.
    const HIGH_ZONE_THRESHOLD: f32 = 0.90;
    const MEDIUM_ZONE_THRESHOLD: f32 = 0.75;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn studio_2_has_sixteen_slots_in_two_columns() {
        let g = Geometry::STUDIO_2;
        assert_eq!(g.slot_count(), 16);
        assert_eq!(g.columns, 2);
        assert_eq!(g.slots().count(), 16);
    }

    #[test]
    fn home_has_thirty_slots_in_three_columns() {
        // Per Gardyn's own setup documentation: three columns, ten slots each.
        let g = Geometry::HOME;
        assert_eq!(g.slot_count(), 30);
        assert_eq!(g.columns, 3);
        assert_eq!(g.rows_per_column, 10);
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
    fn columns_and_positions_agree() {
        let g = Geometry::HOME;
        for slot in g.slots() {
            let p = g.position(slot).unwrap();
            assert_eq!(g.slot_at(p.column, p.row), Some(slot));
        }
    }

    #[test]
    fn a_column_yields_its_slots_top_to_bottom() {
        let g = Geometry::STUDIO_2;
        let first: Vec<_> = g.column(0).collect();
        assert_eq!(first.len(), 8);
        assert_eq!(first[0], SlotId(0));
        assert_eq!(first[7], SlotId(7));

        let second: Vec<_> = g.column(1).collect();
        assert_eq!(second[0], SlotId(8));
    }

    #[test]
    fn an_out_of_range_column_is_empty() {
        assert_eq!(Geometry::STUDIO_2.column(5).count(), 0);
    }

    #[test]
    fn out_of_range_slot_has_no_position() {
        assert_eq!(Geometry::STUDIO_2.position(SlotId(16)), None);
    }

    #[test]
    fn light_peaks_at_the_middle_of_a_column_not_the_top() {
        // Gardyn's placement guide puts fruiting plants "in the center of your Gardyn,
        // where the light reaches maximum intensity".
        let g = Geometry::STUDIO_2;
        let top = g.position(SlotId(0)).unwrap().light_exposure(&g);
        let middle = g.position(SlotId(4)).unwrap().light_exposure(&g);
        let bottom = g.position(SlotId(7)).unwrap().light_exposure(&g);

        assert!(middle > top, "middle {middle} should beat top {top}");
        assert!(middle > bottom, "middle {middle} should beat bottom {bottom}");
    }

    #[test]
    fn the_two_ends_of_a_column_are_roughly_equal() {
        let g = Geometry::STUDIO_2;
        let top = g.position(SlotId(0)).unwrap().light_exposure(&g);
        let bottom = g.position(SlotId(7)).unwrap().light_exposure(&g);
        assert!((top - bottom).abs() < 0.05);
    }

    #[test]
    fn the_middle_column_of_a_home_beats_the_outer_ones() {
        let g = Geometry::HOME;
        let outer = g.position(g.slot_at(0, 5).unwrap()).unwrap().light_exposure(&g);
        let middle = g.position(g.slot_at(1, 5).unwrap()).unwrap().light_exposure(&g);
        let far = g.position(g.slot_at(2, 5).unwrap()).unwrap().light_exposure(&g);

        assert!(middle > outer);
        assert!((outer - far).abs() < 1e-5, "the two outer columns should match");
    }

    #[test]
    fn a_two_column_tower_has_no_favoured_column() {
        // With two columns there is no "middle", so nothing should distinguish them.
        let g = Geometry::STUDIO_2;
        let a = g.position(g.slot_at(0, 4).unwrap()).unwrap().light_exposure(&g);
        let b = g.position(g.slot_at(1, 4).unwrap()).unwrap().light_exposure(&g);
        assert_eq!(a, b);
    }

    #[test]
    fn every_position_reports_a_zone_and_the_brightest_is_high() {
        let g = Geometry::STUDIO_2;
        let middle = g.position(SlotId(4)).unwrap().light_zone(&g);
        let end = g.position(SlotId(0)).unwrap().light_zone(&g);
        assert_eq!(middle, LightZone::High);
        assert!(end < LightZone::High, "the ends should not be high light");
    }

    #[test]
    fn a_studio_2_uses_gardens_published_zone_map() {
        let g = Geometry::STUDIO_2;
        let mut counts = std::collections::BTreeMap::new();
        for slot in g.slots() {
            *counts.entry(g.light_zone(slot)).or_insert(0) += 1;
        }
        assert_eq!(counts.get(&LightZone::High), Some(&5));
        assert_eq!(counts.get(&LightZone::Medium), Some(&9));
        assert_eq!(counts.get(&LightZone::Low), Some(&2));
    }

    #[test]
    fn the_published_zones_are_staggered_between_the_columns() {
        // The detail that rules out any smooth top-to-bottom model: the left column
        // has its bright slots on even rows and the right column on odd ones.
        let g = Geometry::STUDIO_2;
        let high_rows = |column: u8| -> Vec<u8> {
            g.column(column)
                .filter(|s| g.light_zone(*s) == LightZone::High)
                .filter_map(|s| g.position(s).map(|p| p.row))
                .collect()
        };
        assert_eq!(high_rows(0), vec![2, 4, 6]);
        assert_eq!(high_rows(1), vec![3, 5]);
    }

    #[test]
    fn exposure_agrees_with_the_published_zone() {
        // Growth and placement advice must not disagree about which slots are bright.
        let g = Geometry::STUDIO_2;
        for slot in g.slots() {
            let expected = match g.light_zone(slot) {
                LightZone::High => 1.0,
                LightZone::Medium => 0.85,
                LightZone::Low => 0.68,
            };
            assert_eq!(g.light_exposure(slot), expected);
        }
    }

    #[test]
    fn a_home_falls_back_to_the_modelled_curve() {
        // No published map for the Home line, so the centre-peaked model stands in.
        let g = Geometry::HOME;
        assert!(g.zone_map().is_none());
        let middle = g.light_exposure(g.slot_at(1, 5).unwrap());
        let corner = g.light_exposure(g.slot_at(0, 0).unwrap());
        assert!(middle > corner);
    }

    #[test]
    fn all_three_zones_exist_on_a_home_tower() {
        let g = Geometry::HOME;
        let zones: std::collections::BTreeSet<_> = g
            .slots()
            .filter_map(|s| g.position(s))
            .map(|p| p.light_zone(&g))
            .collect();
        assert_eq!(zones.len(), 3, "a 30-slot tower should span all three zones");
    }

    #[test]
    fn a_brighter_slot_satisfies_a_dimmer_plant_but_not_the_reverse() {
        assert!(LightZone::Low.satisfied_by(LightZone::High));
        assert!(LightZone::Medium.satisfied_by(LightZone::Medium));
        assert!(!LightZone::High.satisfied_by(LightZone::Medium));
        assert!(!LightZone::High.satisfied_by(LightZone::Low));
    }

    #[test]
    fn slot_display_is_one_based_for_humans() {
        assert_eq!(SlotId(0).to_string(), "slot 1");
    }
}
