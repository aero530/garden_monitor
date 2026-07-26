//! A planting: one yCube occupying one slot for one life cycle.

use crate::slot::SlotId;
use crate::time::{days_between, days_since_or_never};
use crate::variety::{Variety, VarietyId};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlantingId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Sown, no sprout observed yet.
    Seeded,
    /// Sprouted but not yet thinned to final count.
    Seedling,
    /// Growing out, not yet yielding.
    Vegetative,
    /// Producing.
    Mature,
    /// Yield falling off; plan a replacement.
    Declining,
    /// Past useful life; the slot should be recycled.
    Spent,
}

impl Stage {
    pub fn is_producing(self) -> bool {
        matches!(self, Stage::Mature | Stage::Declining)
    }

    pub fn label(self) -> &'static str {
        match self {
            Stage::Seeded => "seeded",
            Stage::Seedling => "seedling",
            Stage::Vegetative => "vegetative",
            Stage::Mature => "mature",
            Stage::Declining => "declining",
            Stage::Spent => "spent",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Planting {
    pub id: PlantingId,
    pub slot: SlotId,
    pub variety: VarietyId,
    pub planted_at: Timestamp,
    pub germinated_at: Option<Timestamp>,
    pub thinned_at: Option<Timestamp>,
    pub last_root_check: Option<Timestamp>,
    pub last_prune: Option<Timestamp>,
    pub last_harvest: Option<Timestamp>,
    pub harvest_count: u32,
    /// Set when the plant is pulled; the planting stays in history for yield stats.
    pub removed_at: Option<Timestamp>,
}

impl Planting {
    pub fn new(id: PlantingId, slot: SlotId, variety: VarietyId, planted_at: Timestamp) -> Self {
        Self {
            id,
            slot,
            variety,
            planted_at,
            germinated_at: None,
            thinned_at: None,
            last_root_check: None,
            last_prune: None,
            last_harvest: None,
            harvest_count: 0,
            removed_at: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.removed_at.is_none()
    }

    pub fn age_days(&self, now: Timestamp) -> f64 {
        days_between(self.planted_at, now)
    }

    /// Days since the sprout was recorded, or `None` if it has not germinated.
    pub fn days_since_germination(&self, now: Timestamp) -> Option<f64> {
        self.germinated_at.map(|g| days_between(g, now))
    }

    pub fn days_since_root_check(&self, now: Timestamp) -> f64 {
        days_since_or_never(self.last_root_check, now)
    }

    pub fn days_since_harvest(&self, now: Timestamp) -> f64 {
        days_since_or_never(self.last_harvest, now)
    }

    pub fn days_since_prune(&self, now: Timestamp) -> f64 {
        days_since_or_never(self.last_prune, now)
    }

    /// Fraction of the variety's productive life consumed, measured from germination.
    /// Values above 1.0 mean the planting is overdue for replacement.
    pub fn life_fraction(&self, variety: &Variety, now: Timestamp) -> Option<f64> {
        let elapsed = self.days_since_germination(now)?;
        let span = f64::from(variety.productive_life_days);
        (span > 0.0).then(|| elapsed / span)
    }

    pub fn stage(&self, variety: &Variety, now: Timestamp) -> Stage {
        let Some(since_germ) = self.days_since_germination(now) else {
            return Stage::Seeded;
        };
        let life = f64::from(variety.productive_life_days);
        let first_harvest = f64::from(variety.days_to_first_harvest);

        if since_germ >= life {
            Stage::Spent
        } else if since_germ >= life * Self::DECLINE_ONSET {
            Stage::Declining
        } else if since_germ >= first_harvest {
            Stage::Mature
        } else if since_germ >= Self::SEEDLING_DAYS {
            Stage::Vegetative
        } else {
            Stage::Seedling
        }
    }

    /// Yield tails off over the final stretch of a variety's productive life.
    const DECLINE_ONSET: f64 = 0.8;
    /// Days post-germination during which the plant counts as a seedling. Matches the
    /// documented weeks 2-6 thinning window.
    const SEEDLING_DAYS: f64 = 14.0;

    /// When the next harvest is expected, in days since germination. `None` for a
    /// single-harvest variety that has already been taken.
    pub fn next_harvest_at_days(&self, variety: &Variety) -> Option<f64> {
        variety.days_to_harvest_n(self.harvest_count + 1)
    }

    /// Days until the next expected harvest. Negative means it is already due.
    pub fn days_until_harvest(&self, variety: &Variety, now: Timestamp) -> Option<f64> {
        let since_germ = self.days_since_germination(now)?;
        let target = self.next_harvest_at_days(variety)?;
        Some(target - since_germ)
    }

    pub fn needs_thinning(&self, variety: &Variety, now: Timestamp) -> bool {
        // Only meaningful for varieties grown multi-up, and only once sprouted.
        self.thinned_at.is_none()
            && variety.thin_to > 0
            && self
                .days_since_germination(now)
                .is_some_and(|d| (Self::THIN_WINDOW_OPENS..=Self::THIN_WINDOW_CLOSES).contains(&d))
    }

    /// Weeks 2-6 after germination, per the documented care cycle.
    const THIN_WINDOW_OPENS: f64 = 7.0;
    const THIN_WINDOW_CLOSES: f64 = 42.0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::add_days;
    use crate::variety::VarietyBook;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn kale() -> Variety {
        VarietyBook::starter()
            .get(&VarietyId::new("kale-lacinato"))
            .unwrap()
            .clone()
    }

    fn germinated_planting(days_ago: f64) -> Planting {
        let now = t0();
        let mut p = Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new("kale-lacinato"),
            add_days(now, -(days_ago + 6.0)),
        );
        p.germinated_at = Some(add_days(now, -days_ago));
        p
    }

    #[test]
    fn ungerminated_planting_is_seeded() {
        let p = Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new("kale-lacinato"),
            t0(),
        );
        assert_eq!(p.stage(&kale(), t0()), Stage::Seeded);
        assert_eq!(p.days_since_germination(t0()), None);
    }

    #[test]
    fn stage_progresses_with_age() {
        let k = kale();
        assert_eq!(germinated_planting(3.0).stage(&k, t0()), Stage::Seedling);
        assert_eq!(germinated_planting(20.0).stage(&k, t0()), Stage::Vegetative);
        assert_eq!(germinated_planting(40.0).stage(&k, t0()), Stage::Mature);
        // productive_life_days is 150, so decline starts at 120.
        assert_eq!(germinated_planting(130.0).stage(&k, t0()), Stage::Declining);
        assert_eq!(germinated_planting(160.0).stage(&k, t0()), Stage::Spent);
    }

    #[test]
    fn only_mature_and_declining_produce() {
        assert!(Stage::Mature.is_producing());
        assert!(Stage::Declining.is_producing());
        assert!(!Stage::Vegetative.is_producing());
        assert!(!Stage::Spent.is_producing());
    }

    #[test]
    fn harvest_schedule_advances_with_each_harvest() {
        let k = kale();
        let mut p = germinated_planting(35.0);
        // First harvest is due at day 35, so right now.
        assert_eq!(p.days_until_harvest(&k, t0()), Some(0.0));
        p.harvest_count = 1;
        // Next one is 10 days later.
        assert_eq!(p.days_until_harvest(&k, t0()), Some(10.0));
    }

    #[test]
    fn thinning_window_is_weeks_two_to_six() {
        let k = kale();
        assert!(!germinated_planting(3.0).needs_thinning(&k, t0()));
        assert!(germinated_planting(20.0).needs_thinning(&k, t0()));
        assert!(!germinated_planting(50.0).needs_thinning(&k, t0()));
    }

    #[test]
    fn thinning_is_not_requested_twice() {
        let k = kale();
        let mut p = germinated_planting(20.0);
        assert!(p.needs_thinning(&k, t0()));
        p.thinned_at = Some(t0());
        assert!(!p.needs_thinning(&k, t0()));
    }

    #[test]
    fn a_never_checked_planting_is_infinitely_overdue() {
        assert_eq!(germinated_planting(20.0).days_since_root_check(t0()), f64::INFINITY);
    }
}
