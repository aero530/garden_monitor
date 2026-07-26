//! The plant book: per-variety horticultural parameters.
//!
//! Everything the calendar-based rules know about a plant lives here. When
//! `CanopyMetrics` is enabled these become priors rather than the sole source of
//! truth, but they remain the fallback whenever vision is off or a slot is occluded.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VarietyId(pub String);

impl VarietyId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl fmt::Display for VarietyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Herb,
    LeafyGreen,
    Fruiting,
    Flower,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanopyClass {
    Compact,
    Medium,
    Large,
    Vining,
}

/// How a variety yields, which determines whether "harvest" is a one-shot event or a
/// recurring cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "style")]
pub enum HarvestStyle {
    /// Leafy greens and herbs: take outer leaves repeatedly.
    CutAndComeAgain { interval_days: u16 },
    /// Whole-plant harvest ends the planting.
    Single,
    /// Fruiting plants producing in waves once mature.
    ContinuousFruiting { interval_days: u16 },
}

/// Inclusive target band for a measured value.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TargetRange {
    pub min: f32,
    pub max: f32,
}

impl TargetRange {
    pub const fn new(min: f32, max: f32) -> Self {
        Self { min, max }
    }

    pub fn contains(&self, v: f32) -> bool {
        (self.min..=self.max).contains(&v)
    }

    pub fn midpoint(&self) -> f32 {
        (self.min + self.max) / 2.0
    }

    /// Signed distance outside the band; zero when inside.
    pub fn deviation(&self, v: f32) -> f32 {
        if v < self.min {
            v - self.min
        } else if v > self.max {
            v - self.max
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Variety {
    pub id: VarietyId,
    pub name: String,
    pub category: Category,
    /// Typical days from seeding to visible sprout.
    pub germination_days: u16,
    /// Days from germination to first harvest.
    pub days_to_first_harvest: u16,
    /// Days from germination until yield falls off and the slot should be recycled.
    pub productive_life_days: u16,
    pub canopy: CanopyClass,
    pub harvest_style: HarvestStyle,
    /// Seedlings to thin down to during weeks 2-6.
    pub thin_to: u8,
    pub needs_pruning: bool,
    pub needs_pollination: bool,
    /// Canopy area at which the plant is worth harvesting. Used only when
    /// `CanopyMetrics` is enabled.
    pub harvest_canopy_cm2: Option<f32>,
    /// Used only when a `Conductivity` probe is fitted.
    pub ec_target: Option<TargetRange>,
    /// Used only when a `PotentialHydrogen` probe is fitted.
    pub ph_target: Option<TargetRange>,
}

impl Variety {
    /// Expected days from germination to the given harvest number (1-based).
    pub fn days_to_harvest_n(&self, n: u32) -> Option<f64> {
        let first = f64::from(self.days_to_first_harvest);
        match self.harvest_style {
            HarvestStyle::Single => (n == 1).then_some(first),
            HarvestStyle::CutAndComeAgain { interval_days }
            | HarvestStyle::ContinuousFruiting { interval_days } => {
                Some(first + f64::from(n.saturating_sub(1)) * f64::from(interval_days))
            }
        }
    }

    pub fn harvest_interval_days(&self) -> Option<f64> {
        match self.harvest_style {
            HarvestStyle::Single => None,
            HarvestStyle::CutAndComeAgain { interval_days }
            | HarvestStyle::ContinuousFruiting { interval_days } => Some(f64::from(interval_days)),
        }
    }
}

/// Lookup over all known varieties.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VarietyBook(BTreeMap<VarietyId, Variety>);

impl VarietyBook {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn insert(&mut self, v: Variety) {
        self.0.insert(v.id.clone(), v);
    }

    pub fn get(&self, id: &VarietyId) -> Option<&Variety> {
        self.0.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Variety> {
        self.0.values()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// A starter book of common Gardyn varieties, enough to drive the simulator and
    /// the rule tests. The production book loads from `data/varieties.json`; these
    /// figures are typical published values and should be refined against observed
    /// growth once `CanopyMetrics` has a season of history.
    pub fn starter() -> Self {
        let mut book = Self::new();
        for v in starter_varieties() {
            book.insert(v);
        }
        book
    }
}

impl FromIterator<Variety> for VarietyBook {
    fn from_iter<I: IntoIterator<Item = Variety>>(iter: I) -> Self {
        let mut book = Self::new();
        for v in iter {
            book.insert(v);
        }
        book
    }
}

fn starter_varieties() -> Vec<Variety> {
    // Nutrient bands are conventional hydroponic ranges: leafy crops run leaner than
    // fruiting crops, which is exactly the distinction an EC probe would let us act on.
    let leafy_ec = Some(TargetRange::new(0.8, 1.4));
    let herb_ec = Some(TargetRange::new(1.0, 1.6));
    let fruit_ec = Some(TargetRange::new(2.0, 3.5));
    let ph = Some(TargetRange::new(5.5, 6.5));

    vec![
        Variety {
            id: VarietyId::new("basil-genovese"),
            name: "Genovese Basil".into(),
            category: Category::Herb,
            germination_days: 7,
            days_to_first_harvest: 28,
            productive_life_days: 120,
            canopy: CanopyClass::Medium,
            harvest_style: HarvestStyle::CutAndComeAgain { interval_days: 14 },
            thin_to: 3,
            needs_pruning: true,
            needs_pollination: false,
            harvest_canopy_cm2: Some(320.0),
            ec_target: herb_ec,
            ph_target: ph,
        },
        Variety {
            id: VarietyId::new("lettuce-butterhead"),
            name: "Butterhead Lettuce".into(),
            category: Category::LeafyGreen,
            germination_days: 5,
            days_to_first_harvest: 30,
            productive_life_days: 75,
            canopy: CanopyClass::Medium,
            harvest_style: HarvestStyle::CutAndComeAgain { interval_days: 12 },
            thin_to: 1,
            needs_pruning: false,
            needs_pollination: false,
            harvest_canopy_cm2: Some(400.0),
            ec_target: leafy_ec,
            ph_target: ph,
        },
        Variety {
            id: VarietyId::new("kale-lacinato"),
            name: "Lacinato Kale".into(),
            category: Category::LeafyGreen,
            germination_days: 6,
            days_to_first_harvest: 35,
            productive_life_days: 150,
            canopy: CanopyClass::Large,
            harvest_style: HarvestStyle::CutAndComeAgain { interval_days: 10 },
            thin_to: 1,
            needs_pruning: true,
            needs_pollination: false,
            harvest_canopy_cm2: Some(520.0),
            ec_target: leafy_ec,
            ph_target: ph,
        },
        Variety {
            id: VarietyId::new("arugula"),
            name: "Arugula".into(),
            category: Category::LeafyGreen,
            germination_days: 4,
            days_to_first_harvest: 21,
            productive_life_days: 60,
            canopy: CanopyClass::Compact,
            harvest_style: HarvestStyle::CutAndComeAgain { interval_days: 9 },
            thin_to: 3,
            needs_pruning: false,
            needs_pollination: false,
            harvest_canopy_cm2: Some(220.0),
            ec_target: leafy_ec,
            ph_target: ph,
        },
        Variety {
            id: VarietyId::new("cilantro"),
            name: "Cilantro".into(),
            category: Category::Herb,
            germination_days: 8,
            days_to_first_harvest: 26,
            productive_life_days: 70,
            canopy: CanopyClass::Compact,
            harvest_style: HarvestStyle::CutAndComeAgain { interval_days: 12 },
            thin_to: 3,
            needs_pruning: false,
            needs_pollination: false,
            harvest_canopy_cm2: Some(200.0),
            ec_target: herb_ec,
            ph_target: ph,
        },
        Variety {
            id: VarietyId::new("swiss-chard"),
            name: "Swiss Chard".into(),
            category: Category::LeafyGreen,
            germination_days: 7,
            days_to_first_harvest: 35,
            productive_life_days: 140,
            canopy: CanopyClass::Large,
            harvest_style: HarvestStyle::CutAndComeAgain { interval_days: 11 },
            thin_to: 1,
            needs_pruning: false,
            needs_pollination: false,
            harvest_canopy_cm2: Some(480.0),
            ec_target: leafy_ec,
            ph_target: ph,
        },
        Variety {
            id: VarietyId::new("bok-choy"),
            name: "Bok Choy".into(),
            category: Category::LeafyGreen,
            germination_days: 5,
            days_to_first_harvest: 32,
            productive_life_days: 50,
            canopy: CanopyClass::Medium,
            harvest_style: HarvestStyle::Single,
            thin_to: 1,
            needs_pruning: false,
            needs_pollination: false,
            harvest_canopy_cm2: Some(380.0),
            ec_target: leafy_ec,
            ph_target: ph,
        },
        Variety {
            id: VarietyId::new("tomato-cherry"),
            name: "Cherry Tomato".into(),
            category: Category::Fruiting,
            germination_days: 8,
            days_to_first_harvest: 60,
            productive_life_days: 210,
            canopy: CanopyClass::Vining,
            harvest_style: HarvestStyle::ContinuousFruiting { interval_days: 7 },
            thin_to: 1,
            needs_pruning: true,
            needs_pollination: true,
            harvest_canopy_cm2: Some(900.0),
            ec_target: fruit_ec,
            ph_target: ph,
        },
        Variety {
            id: VarietyId::new("pepper-jalapeno"),
            name: "Jalapeño Pepper".into(),
            category: Category::Fruiting,
            germination_days: 12,
            days_to_first_harvest: 70,
            productive_life_days: 200,
            canopy: CanopyClass::Large,
            harvest_style: HarvestStyle::ContinuousFruiting { interval_days: 10 },
            thin_to: 1,
            needs_pruning: true,
            needs_pollination: true,
            harvest_canopy_cm2: Some(700.0),
            ec_target: fruit_ec,
            ph_target: ph,
        },
        Variety {
            id: VarietyId::new("nasturtium"),
            name: "Nasturtium".into(),
            category: Category::Flower,
            germination_days: 10,
            days_to_first_harvest: 45,
            productive_life_days: 120,
            canopy: CanopyClass::Vining,
            harvest_style: HarvestStyle::CutAndComeAgain { interval_days: 14 },
            thin_to: 3,
            needs_pruning: true,
            needs_pollination: false,
            harvest_canopy_cm2: Some(450.0),
            ec_target: leafy_ec,
            ph_target: ph,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_book_is_populated_and_addressable() {
        let book = VarietyBook::starter();
        assert_eq!(book.len(), 10);
        let basil = book.get(&VarietyId::new("basil-genovese")).unwrap();
        assert_eq!(basil.category, Category::Herb);
        assert!(basil.needs_pruning);
    }

    #[test]
    fn cut_and_come_again_repeats_on_its_interval() {
        let book = VarietyBook::starter();
        let kale = book.get(&VarietyId::new("kale-lacinato")).unwrap();
        assert_eq!(kale.days_to_harvest_n(1), Some(35.0));
        assert_eq!(kale.days_to_harvest_n(2), Some(45.0));
        assert_eq!(kale.days_to_harvest_n(3), Some(55.0));
    }

    #[test]
    fn single_harvest_has_no_second_yield() {
        let book = VarietyBook::starter();
        let bok = book.get(&VarietyId::new("bok-choy")).unwrap();
        assert_eq!(bok.days_to_harvest_n(1), Some(32.0));
        assert_eq!(bok.days_to_harvest_n(2), None);
        assert_eq!(bok.harvest_interval_days(), None);
    }

    #[test]
    fn fruiting_varieties_want_a_richer_solution_than_greens() {
        let book = VarietyBook::starter();
        let tomato = book.get(&VarietyId::new("tomato-cherry")).unwrap();
        let lettuce = book.get(&VarietyId::new("lettuce-butterhead")).unwrap();
        assert!(tomato.ec_target.unwrap().min > lettuce.ec_target.unwrap().max);
    }

    #[test]
    fn target_range_deviation_is_signed_and_zero_inside() {
        let r = TargetRange::new(1.0, 2.0);
        assert_eq!(r.deviation(1.5), 0.0);
        assert_eq!(r.deviation(0.5), -0.5);
        assert_eq!(r.deviation(2.5), 0.5);
        assert!(r.contains(1.0) && r.contains(2.0));
    }
}
