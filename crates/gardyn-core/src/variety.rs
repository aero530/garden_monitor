//! The plant book: Gardyn's yCube catalogue.
//!
//! Loaded from `data/varieties.json`, transcribed from Gardyn's placement guide and
//! the 134 per-plant articles it links. Sprout days, maturity days, thinning counts,
//! pollination and pruning requirements, care level, plant size and light zone are all
//! Gardyn's own published figures.
//!
//! Two things are **derived**, because Gardyn does not publish them, and both are
//! marked as such at the point of derivation: productive lifespan, and harvest cadence.

use crate::slot::LightZone;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// The catalogue, embedded so the binary needs no data files alongside it.
const CATALOGUE: &str = include_str!("../data/varieties.json");

/// Gardyn's own prose for each plant: what it is, and how to look after it.
///
/// Held separately from the structured catalogue because it is long, optional, and
/// arrives one article at a time. A variety with no entry here still works; it simply
/// shows no description.
const DETAILS: &str = include_str!("../data/variety-details.json");

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

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Herb => "herb",
            Category::LeafyGreen => "green",
            Category::Fruiting => "fruiting",
            Category::Flower => "flower",
        }
    }
}

/// Gardyn's own difficulty rating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CareLevel {
    Beginner,
    Intermediate,
    Advanced,
}

impl CareLevel {
    pub fn label(self) -> &'static str {
        match self {
            CareLevel::Beginner => "beginner",
            CareLevel::Intermediate => "intermediate",
            CareLevel::Advanced => "advanced",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanopyClass {
    Compact,
    Medium,
    Large,
    Vining,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "style")]
pub enum HarvestStyle {
    CutAndComeAgain { interval_days: u16 },
    Single,
    ContinuousFruiting { interval_days: u16 },
}

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
    /// Where Gardyn says this belongs in the tower.
    pub light_zone: LightZone,
    /// Set when the plant's own article disagrees with the placement guide's grouping.
    pub guide_zone: Option<LightZone>,
    pub germination_days: u16,
    /// Days from germination to first harvest.
    ///
    /// Gardyn publishes "days to maturity" measured from **sowing**; this is that
    /// figure minus the sprout time, because every rule here measures from germination.
    pub days_to_first_harvest: u16,
    /// Derived: Gardyn does not publish a productive lifespan.
    pub productive_life_days: u16,
    pub canopy: CanopyClass,
    /// Derived: Gardyn does not publish a re-harvest cadence.
    pub harvest_style: HarvestStyle,
    pub thin_to: u8,
    pub needs_pruning: bool,
    pub needs_pollination: bool,
    pub care_level: CareLevel,
    /// Gardyn's published "Plant Size", verbatim.
    pub plant_size: String,
    /// Extra placement guidance from the article, where given.
    pub placement_note: Option<String>,
    /// True when the article carried no data block and figures are category defaults.
    pub estimated: bool,
    pub harvest_canopy_cm2: Option<f32>,
    pub ec_target: Option<TargetRange>,
    pub ph_target: Option<TargetRange>,
    /// Gardyn's "Qualities" prose: flavour, nutrition, what it looks like.
    ///
    /// Kept as paragraphs rather than one blob, because Gardyn writes the care
    /// section as labelled entries ("💡 Temperature: …", "✂️ Pruning: …") and that
    /// structure is most of what makes it readable.
    pub qualities: Vec<String>,
    /// Gardyn's "Care & Harvest" prose: temperature, pruning, pests, when to pick.
    pub care: Vec<String>,
    /// The article the prose came from, so the page can link back to the source.
    pub article_url: Option<String>,
}

impl Variety {
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

    /// Whether the plant's own article contradicts the placement guide's grouping.
    pub fn zone_disputed(&self) -> bool {
        self.guide_zone.is_some_and(|g| g != self.light_zone)
    }
}

// --- Loading -------------------------------------------------------------------

#[derive(Deserialize)]
struct Catalogue {
    varieties: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    id: String,
    name: String,
    category: Category,
    zone: LightZone,
    #[serde(default)]
    guide_zone: Option<LightZone>,
    #[serde(default)]
    sprout: Option<[u16; 2]>,
    #[serde(default)]
    maturity: Option<[u16; 2]>,
    #[serde(default)]
    thin_to: Option<u8>,
    #[serde(default)]
    pollinate: Option<bool>,
    #[serde(default)]
    prune: Option<bool>,
    #[serde(default)]
    care: Option<CareLevel>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    single: bool,
    #[serde(default)]
    no_data: bool,
    #[serde(default)]
    placement: Option<String>,
}

impl Entry {
    fn into_variety(self) -> Variety {
        let sprout = self.sprout.unwrap_or(default_sprout(self.category));
        let maturity = self.maturity.unwrap_or(default_maturity(self.category));

        // Gardyn counts maturity from sowing; the rules count from germination.
        // Subtracting the sprout time converts between the two. The floor stops a
        // fast-sprouting, fast-maturing green from claiming a same-day harvest.
        let days_to_first_harvest = maturity[0].saturating_sub(sprout[0]).max(14);

        let canopy = canopy_from_size(self.size.as_deref(), self.category);

        Variety {
            id: VarietyId(self.id),
            name: self.name,
            category: self.category,
            light_zone: self.zone,
            guide_zone: self.guide_zone,
            // Midpoint of the published window: the earlier bound alone would have the
            // germination-check rule nagging about every cube that is merely average.
            // Floored at one day for bare-root stock, which has no germination stage
            // at all but still needs a non-zero gate for the stage machine.
            germination_days: (sprout[0] + sprout[1]).div_ceil(2).max(1),
            days_to_first_harvest,
            // Derived. Gardyn publishes no lifespan, so this is "maturity plus a
            // category-typical producing window".
            productive_life_days: days_to_first_harvest + producing_window(self.category),
            canopy,
            harvest_style: if self.single {
                HarvestStyle::Single
            } else {
                harvest_style(self.category)
            },
            thin_to: self.thin_to.unwrap_or(1).max(1),
            needs_pruning: self.prune.unwrap_or(true),
            needs_pollination: self.pollinate.unwrap_or(false),
            care_level: self.care.unwrap_or(CareLevel::Intermediate),
            plant_size: self.size.unwrap_or_else(|| "unknown".into()),
            placement_note: self.placement,
            estimated: self.no_data,
            harvest_canopy_cm2: Some(canopy_area(canopy)),
            ec_target: Some(ec_target(self.category)),
            ph_target: Some(TargetRange::new(5.5, 6.5)),
            qualities: Vec::new(),
            care: Vec::new(),
            article_url: None,
        }
    }
}

/// Whether Gardyn's prose has been transcribed for this variety yet.
impl Variety {
    pub fn has_description(&self) -> bool {
        !self.qualities.is_empty() || !self.care.is_empty()
    }
}

#[derive(Deserialize)]
struct DetailFile {
    details: BTreeMap<String, Detail>,
}

#[derive(Deserialize)]
struct Detail {
    #[serde(default)]
    qualities: Vec<String>,
    #[serde(default)]
    care: Vec<String>,
    #[serde(default)]
    source: Option<String>,
}

fn default_sprout(category: Category) -> [u16; 2] {
    match category {
        Category::LeafyGreen => [5, 14],
        Category::Herb => [7, 21],
        Category::Fruiting => [10, 21],
        Category::Flower => [7, 18],
    }
}

fn default_maturity(category: Category) -> [u16; 2] {
    match category {
        Category::LeafyGreen => [45, 55],
        Category::Herb => [60, 75],
        Category::Fruiting => [70, 90],
        Category::Flower => [55, 70],
    }
}

/// Derived: how long a category keeps producing after it starts.
fn producing_window(category: Category) -> u16 {
    match category {
        Category::LeafyGreen => 45,
        Category::Herb => 90,
        Category::Fruiting => 120,
        Category::Flower => 60,
    }
}

/// Derived: how often a category can be picked again.
fn harvest_style(category: Category) -> HarvestStyle {
    match category {
        Category::Fruiting => HarvestStyle::ContinuousFruiting { interval_days: 7 },
        Category::Flower => HarvestStyle::CutAndComeAgain { interval_days: 14 },
        _ => HarvestStyle::CutAndComeAgain { interval_days: 12 },
    }
}

fn canopy_from_size(size: Option<&str>, category: Category) -> CanopyClass {
    let normalised = size.unwrap_or("").to_lowercase().replace(' ', "");
    if category == Category::Fruiting && normalised.contains("2ft") {
        return CanopyClass::Vining;
    }
    match () {
        _ if normalised.starts_with("<1") => CanopyClass::Compact,
        _ if normalised.starts_with("1ft") || normalised.starts_with("1-2") => CanopyClass::Medium,
        _ if normalised.starts_with("<2") => CanopyClass::Medium,
        _ if normalised.contains("3ft") => CanopyClass::Vining,
        _ if normalised.starts_with("2") => CanopyClass::Large,
        _ => CanopyClass::Medium,
    }
}

/// Projected canopy area at which a plant is worth picking.
fn canopy_area(canopy: CanopyClass) -> f32 {
    match canopy {
        CanopyClass::Compact => 220.0,
        CanopyClass::Medium => 380.0,
        CanopyClass::Large => 520.0,
        CanopyClass::Vining => 900.0,
    }
}

/// Conventional hydroponic bands: leafy crops run leaner than fruiting crops.
fn ec_target(category: Category) -> TargetRange {
    match category {
        Category::LeafyGreen => TargetRange::new(0.8, 1.4),
        Category::Herb => TargetRange::new(1.0, 1.6),
        Category::Flower => TargetRange::new(1.0, 1.8),
        Category::Fruiting => TargetRange::new(2.0, 3.5),
    }
}

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

    /// Varieties Gardyn places in a given light zone.
    pub fn in_zone(&self, zone: LightZone) -> impl Iterator<Item = &Variety> {
        self.0.values().filter(move |v| v.light_zone == zone)
    }

    /// The full Gardyn catalogue.
    ///
    /// Panics on a malformed catalogue, which is a build-time authoring error rather
    /// than anything a running system can encounter — the file is embedded.
    pub fn gardyn() -> Self {
        let parsed: Catalogue =
            serde_json::from_str(CATALOGUE).expect("embedded variety catalogue is valid JSON");
        let detail: DetailFile =
            serde_json::from_str(DETAILS).expect("embedded variety details are valid JSON");

        let mut book: Self = parsed.varieties.into_iter().map(Entry::into_variety).collect();
        for (id, d) in detail.details {
            // A detail entry for an id that is not in the catalogue is ignored rather
            // than being an error: the two files are transcribed independently.
            if let Some(variety) = book.0.get_mut(&VarietyId(id)) {
                variety.qualities = d.qualities;
                variety.care = d.care;
                variety.article_url = d.source;
            }
        }
        book
    }

    /// How many varieties have Gardyn's prose transcribed.
    ///
    /// Surfaced so the catalogue page can be honest about coverage instead of
    /// silently showing blanks.
    pub fn described_count(&self) -> usize {
        self.0.values().filter(|v| v.has_description()).count()
    }

    /// Alias kept for call sites that predate the full catalogue.
    pub fn starter() -> Self {
        Self::gardyn()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_whole_catalogue_parses() {
        let book = VarietyBook::gardyn();
        assert_eq!(book.len(), 135, "expected Gardyn's full yCube list");
    }

    #[test]
    fn every_variety_has_a_usable_schedule() {
        for v in VarietyBook::gardyn().iter() {
            assert!(v.germination_days > 0, "{} has no sprout time", v.name);
            assert!(
                v.days_to_first_harvest >= 14,
                "{} harvests implausibly fast",
                v.name
            );
            assert!(
                v.productive_life_days > v.days_to_first_harvest,
                "{} dies before it yields",
                v.name
            );
            assert!(v.thin_to >= 1, "{} thins to zero plants", v.name);
        }
    }

    #[test]
    fn maturity_is_converted_from_sowing_to_germination() {
        // Gardyn lists Lacinato Kale as 7-21 days to sprout and 65 to maturity, and
        // measures maturity from sowing. From germination that is 65 - 7 = 58.
        let book = VarietyBook::gardyn();
        let kale = book.get(&VarietyId::new("kale-lacinato")).unwrap();
        assert_eq!(kale.days_to_first_harvest, 58);
        assert_eq!(kale.germination_days, 14);
    }

    #[test]
    fn the_three_light_zones_are_all_populated() {
        let book = VarietyBook::gardyn();
        for zone in LightZone::ALL {
            assert!(
                book.in_zone(*zone).count() > 5,
                "{zone} has almost nothing in it"
            );
        }
    }

    #[test]
    fn fruiting_plants_are_flagged_for_pollination_and_greens_are_not() {
        let book = VarietyBook::gardyn();
        assert!(book.get(&VarietyId::new("jalapeno")).unwrap().needs_pollination);
        assert!(book.get(&VarietyId::new("red-cherry-tomato")).unwrap().needs_pollination);
        assert!(!book.get(&VarietyId::new("romaine")).unwrap().needs_pollination);
        assert!(!book.get(&VarietyId::new("mint")).unwrap().needs_pollination);
    }

    #[test]
    fn head_crops_are_harvested_once() {
        let book = VarietyBook::gardyn();
        let iceberg = book.get(&VarietyId::new("iceberg")).unwrap();
        assert_eq!(iceberg.harvest_style, HarvestStyle::Single);
        assert_eq!(iceberg.days_to_harvest_n(2), None);

        let romaine = book.get(&VarietyId::new("romaine")).unwrap();
        assert!(matches!(
            romaine.harvest_style,
            HarvestStyle::CutAndComeAgain { .. }
        ));
        assert!(romaine.days_to_harvest_n(3).is_some());
    }

    #[test]
    fn zone_conflicts_between_the_guide_and_the_plant_page_are_recorded() {
        // Purple Beans sit under High Light in the placement guide, but their own
        // article says Medium. Both are kept so the disagreement is visible.
        let book = VarietyBook::gardyn();
        let beans = book.get(&VarietyId::new("purple-beans")).unwrap();
        assert_eq!(beans.light_zone, LightZone::Medium);
        assert_eq!(beans.guide_zone, Some(LightZone::High));
        assert!(beans.zone_disputed());

        let kale = book.get(&VarietyId::new("kale-lacinato")).unwrap();
        assert!(!kale.zone_disputed());
    }

    #[test]
    fn varieties_with_no_published_data_are_marked_estimated() {
        let book = VarietyBook::gardyn();
        // Gardyn's American Mustard article 404s and Sorrel's carries no data block.
        assert!(book.get(&VarietyId::new("american-mustard")).unwrap().estimated);
        assert!(book.get(&VarietyId::new("sorrel")).unwrap().estimated);
        assert!(!book.get(&VarietyId::new("basil")).unwrap().estimated);
    }

    #[test]
    fn placement_notes_survive_the_import() {
        let book = VarietyBook::gardyn();
        let cucumber = book.get(&VarietyId::new("cucumber")).unwrap();
        assert!(cucumber.placement_note.as_deref().unwrap().contains("middle column"));
    }

    #[test]
    fn fruiting_varieties_want_a_richer_solution_than_greens() {
        let book = VarietyBook::gardyn();
        let tomato = book.get(&VarietyId::new("red-cherry-tomato")).unwrap();
        let lettuce = book.get(&VarietyId::new("butterhead")).unwrap();
        assert!(tomato.ec_target.unwrap().min > lettuce.ec_target.unwrap().max);
    }

    #[test]
    fn cut_and_come_again_repeats_on_its_interval() {
        let book = VarietyBook::gardyn();
        let kale = book.get(&VarietyId::new("kale-lacinato")).unwrap();
        assert_eq!(kale.days_to_harvest_n(1), Some(58.0));
        assert_eq!(kale.days_to_harvest_n(2), Some(70.0));
    }

    #[test]
    fn target_range_deviation_is_signed_and_zero_inside() {
        let r = TargetRange::new(1.0, 2.0);
        assert_eq!(r.deviation(1.5), 0.0);
        assert_eq!(r.deviation(0.5), -0.5);
        assert_eq!(r.deviation(2.5), 0.5);
    }
}
