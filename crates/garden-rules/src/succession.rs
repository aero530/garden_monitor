//! What to plant in a slot, so the harvests do not all arrive at once.
//!
//! Sixteen slots planted on the same afternoon produce sixteen harvests in the same
//! week, and then nothing for a month. That is the failure this exists to avoid, and it
//! is entirely a planning problem: by the time the rules are telling you to harvest, the
//! decision that caused the pile-up was made six weeks earlier.
//!
//! **A first cut, and deliberately a greedy one.** It answers "what should go in *this*
//! slot, today", not "what is the optimal filling of the whole tower over a season".
//! Greedy is the right shape for how a Gardyn is actually used — one cube goes in when
//! one comes out — and a joint optimiser would be answering a question nobody asked.
//!
//! Advisory throughout. Nothing here emits a task or changes state; it ranks candidates
//! and explains why.

use garden_core::{
    Category, GardenState, LightZone, SlotId, TargetRange, Variety, VarietyId, time::days_between,
};

/// One thing worth planting here, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub variety: VarietyId,
    pub name: String,
    /// Days from planting today to the first harvest — germination included, because
    /// that is the number an operator experiences.
    pub harvest_in_days: f64,
    /// Days between this harvest and the nearest one already expected. The score.
    pub clearance_days: f64,
    pub reason: String,
}

/// How many days either side of an existing harvest counts as "the same week".
///
/// Below this the new plant lands on top of something already coming, which is the
/// clustering this exists to prevent.
pub const CROWDED_DAYS: f64 = 7.0;

/// Rank what could go in `slot`, best first.
///
/// Returns nothing when the slot is occupied — replacing a living plant is a different
/// decision, and one the replant rule raises on its own terms.
pub fn suggest(state: &GardenState, slot: SlotId, limit: usize) -> Vec<Suggestion> {
    suggest_around(state, slot, limit, &[])
}

/// The same, but also avoiding harvests that are only planned.
///
/// [`plan_tower`] needs this. Scoring every empty slot independently gives every one of
/// them the same answer — the longest-maturing variety is furthest from what is already
/// growing, for all of them at once — and a plan that says "plant thirteen lemongrass"
/// produces exactly the pile-up it was meant to prevent.
pub fn suggest_around(
    state: &GardenState,
    slot: SlotId,
    limit: usize,
    pending: &[f64],
) -> Vec<Suggestion> {
    if state.planting_in(slot).is_some() {
        return Vec::new();
    }
    let zone = state.geometry.light_zone(slot);
    let mut existing = expected_harvests(state);
    existing.extend_from_slice(pending);
    let growing: Vec<&VarietyId> = state.active_plantings().map(|p| &p.variety).collect();
    let tank = tank_band(state);

    let mut ranked: Vec<Suggestion> = state
        .varieties
        .iter()
        .filter(|v| v.light_zone.satisfied_by(zone))
        .filter(|v| fits_tank(v, tank))
        .map(|v| score(v, zone, &existing, &growing))
        .collect();

    // Clearance first; then, among equals, something not already growing. A tower of
    // one variety is a monoculture whose harvests are identical anyway.
    ranked.sort_by(|a, b| {
        b.clearance_days
            .partial_cmp(&a.clearance_days)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    ranked.truncate(limit);
    ranked
}

/// What is already on the way, soonest first: variety name and days from now.
///
/// Public because anything *showing* a plan needs to show what it planned around, and
/// a display that computed "coming harvests" its own way would disagree with the
/// planner — which is worse than not showing it, because the advice would look wrong.
///
/// Plants that have not germinated are included at their calendar estimate. The whole
/// point is to plan around what is coming, and something sown last week is very much
/// coming; `Planting::days_until_harvest` returns nothing for them because it measures
/// from germination, so this falls back to counting from the sowing date.
pub fn coming_harvests(state: &GardenState) -> Vec<(String, f64)> {
    let mut coming: Vec<(String, f64)> = state
        .planted()
        .filter_map(|(planting, variety)| {
            let days = match planting.germinated_at {
                Some(_) => planting.days_until_harvest(variety, state.now)?,
                None => {
                    let elapsed = days_between(planting.planted_at, state.now);
                    f64::from(variety.germination_days)
                        + f64::from(variety.days_to_first_harvest)
                        - elapsed
                }
            };
            (days > 0.0).then(|| (variety.name.clone(), days))
        })
        .collect();
    coming.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    coming
}

fn expected_harvests(state: &GardenState) -> Vec<f64> {
    coming_harvests(state).into_iter().map(|(_, d)| d).collect()
}

/// The EC band the tank can actually hold, if anything is growing.
///
/// Reuses the same reasoning as the dosing rule: a tank is one solution, so a variety
/// whose band does not overlap what is already in there cannot be satisfied at the same
/// time as its neighbours. Suggesting a tomato into a tank of lettuce is proposing a
/// compromise that starves both.
fn tank_band(state: &GardenState) -> Option<TargetRange> {
    let mut lowest_max = f32::MAX;
    let mut highest_min = f32::MIN;
    let mut any = false;
    for (_, variety) in state.planted() {
        if let Some(range) = variety.ec_target {
            any = true;
            lowest_max = lowest_max.min(range.max);
            highest_min = highest_min.max(range.min);
        }
    }
    any.then_some(TargetRange::new(highest_min, lowest_max))
}

fn fits_tank(variety: &Variety, band: Option<TargetRange>) -> bool {
    let (Some(band), Some(want)) = (band, variety.ec_target) else {
        return true;
    };
    // Overlap, not containment: two ranges that share any strength can both be fed.
    want.min <= band.max && want.max >= band.min
}

fn score(
    variety: &Variety,
    zone: LightZone,
    existing: &[f64],
    growing: &[&VarietyId],
) -> Suggestion {
    let harvest_in =
        f64::from(variety.germination_days) + f64::from(variety.days_to_first_harvest);

    // Distance to the nearest harvest already expected. With an empty garden there is
    // nothing to collide with, so everything is equally clear.
    let clearance = existing
        .iter()
        .map(|d| (d - harvest_in).abs())
        .fold(f64::INFINITY, f64::min);

    let already = growing.contains(&&variety.id);
    let reason = if existing.is_empty() {
        format!(
            "first in the tower — ready about {harvest_in:.0} days after planting"
        )
    } else if clearance >= CROWDED_DAYS {
        format!(
            "harvests around day {harvest_in:.0}, {clearance:.0} days clear of your next one"
        )
    } else {
        format!(
            "harvests around day {harvest_in:.0}, within {clearance:.0} days of another — \
             you would be picking two at once"
        )
    };

    Suggestion {
        variety: variety.id.clone(),
        name: variety.name.clone(),
        harvest_in_days: harvest_in,
        // A variety already in the tower is a tie-break penalty, not a veto: sometimes
        // more of the same really is what you want.
        clearance_days: if already { clearance - 1.0 } else { clearance },
        reason: match (already, zone) {
            (true, _) => format!("{reason}; you already grow this"),
            _ => reason,
        },
    }
}

/// Fill every empty slot, each choice aware of the ones before it.
///
/// Greedy rather than optimal, and in the order the slots come. That is the right shape
/// for how a tower is actually filled — one cube at a time — and the accumulation is
/// what stops the answer being the same variety sixteen times.
pub fn plan_tower(state: &GardenState, per_slot: usize) -> Vec<(SlotId, Vec<Suggestion>)> {
    let mut pending: Vec<f64> = Vec::new();
    let mut plan = Vec::new();

    for slot in state.empty_slots() {
        let ranked = suggest_around(state, slot, per_slot, &pending);
        // The top pick is what this slot is assumed to get, so the next slot plans
        // around it. Taking the whole list would over-constrain on choices not made.
        if let Some(top) = ranked.first() {
            pending.push(top.harvest_in_days);
        }
        plan.push((slot, ranked));
    }
    plan
}

/// One line for a `Replant` task's rationale.
///
/// Empty when there is nothing sensible to say, so the caller can append it
/// unconditionally without producing a dangling "Consider: ".
pub fn replacement_hint(state: &GardenState, slot: SlotId) -> String {
    match suggest(state, slot, 1).first() {
        Some(top) => format!(
            " Consider {} next — {}.",
            top.name,
            top.reason.trim_end_matches('.')
        ),
        None => String::new(),
    }
}

/// Whether a category is a leafy green, for the "what is this" line in the UI.
pub fn category_hint(category: Category) -> &'static str {
    match category {
        Category::LeafyGreen => "quick and forgiving",
        Category::Herb => "cut and come again",
        Category::Fruiting => "slow, needs light and pollinating",
        Category::Flower => "no harvest, but it feeds pollinators",
    }
}

#[cfg(test)]
mod tests {
    use garden_core::{
        GardenState, Geometry, LightZone, Planting, PlantingId, SlotId, Timestamp, VarietyId,
        time::add_days,
    };

    use super::*;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn state() -> GardenState {
        GardenState::new_studio_2(t0())
    }

    fn plant(state: &mut GardenState, slot: u8, variety: &str, planted_days_ago: f64) {
        let id = PlantingId(state.plantings.len() as u64 + 1);
        let mut planting = Planting::new(
            id,
            SlotId(slot),
            VarietyId::new(variety),
            add_days(t0(), -planted_days_ago),
        );
        planting.germinated_at = Some(add_days(t0(), -planted_days_ago + 5.0));
        state.plantings.push(planting);
    }

    #[test]
    fn an_occupied_slot_gets_no_suggestions() {
        // Replacing a living plant is a different decision, and the replant rule
        // raises it on its own terms.
        let mut s = state();
        plant(&mut s, 0, "basil", 10.0);
        assert!(suggest(&s, SlotId(0), 5).is_empty());
    }

    #[test]
    fn nothing_is_suggested_that_the_slot_cannot_light() {
        // The one hard filter that matters: Gardyn's own guide says a high-light plant
        // in a dim slot will sulk, and suggesting one is worse than saying nothing.
        let s = state();
        for slot in s.geometry.slots() {
            let zone = s.geometry.light_zone(slot);
            for suggestion in suggest(&s, slot, 200) {
                let variety = s.varieties.get(&suggestion.variety).unwrap();
                assert!(
                    variety.light_zone.satisfied_by(zone),
                    "{} needs {:?} but {slot} is {zone:?}",
                    variety.name,
                    variety.light_zone
                );
            }
        }
    }

    #[test]
    fn a_low_light_slot_still_has_something_to_offer() {
        // A filter that leaves nothing is a filter that has failed.
        let s = state();
        let dim = s
            .geometry
            .slots()
            .find(|slot| s.geometry.light_zone(*slot) == LightZone::Low)
            .expect("the Studio has a low slot");
        assert!(!suggest(&s, dim, 5).is_empty());
    }

    #[test]
    fn an_empty_garden_says_so_rather_than_inventing_a_clearance() {
        let s = state();
        let top = suggest(&s, SlotId(0), 1).pop().unwrap();
        assert!(top.reason.contains("first in the tower"), "{}", top.reason);
        assert!(top.clearance_days.is_infinite());
    }

    #[test]
    fn suggestions_avoid_landing_on_an_existing_harvest() {
        // The whole point. With one plant harvesting around day 63, the top suggestion
        // must not be something else that also harvests around day 63.
        let mut s = state();
        plant(&mut s, 0, "kale-lacinato", 0.0);

        let existing = expected_harvests(&s)[0];
        let top = suggest(&s, SlotId(1), 1).pop().unwrap();

        assert!(
            (top.harvest_in_days - existing).abs() >= CROWDED_DAYS,
            "suggested {} at day {:.0}, but something already harvests at {:.0}",
            top.name,
            top.harvest_in_days,
            existing
        );
    }

    #[test]
    fn spreading_beats_taking_the_first_legal_variety() {
        // The property that justifies the ranking existing at all.
        let mut s = state();
        plant(&mut s, 0, "kale-lacinato", 0.0);
        let existing = expected_harvests(&s)[0];

        let ranked = suggest(&s, SlotId(1), 200);
        let best = ranked.first().unwrap();
        let arbitrary = ranked.last().unwrap();

        let gap = |sug: &Suggestion| (sug.harvest_in_days - existing).abs();
        assert!(
            gap(best) > gap(arbitrary),
            "best {} ({:.0}) should be further from {existing:.0} than worst {} ({:.0})",
            best.name,
            gap(best),
            arbitrary.name,
            gap(arbitrary)
        );
    }

    #[test]
    fn results_are_ranked_best_first() {
        let mut s = state();
        plant(&mut s, 0, "basil", 0.0);
        let ranked = suggest(&s, SlotId(1), 10);
        assert!(
            ranked.windows(2).all(|w| w[0].clearance_days >= w[1].clearance_days),
            "not sorted"
        );
    }

    #[test]
    fn a_variety_already_growing_loses_a_tie_but_is_not_banned() {
        // Sometimes more of the same really is what you want, so this is a penalty
        // rather than a veto.
        let mut s = state();
        plant(&mut s, 0, "basil", 0.0);
        let ranked = suggest(&s, SlotId(1), 200);
        assert!(
            ranked.iter().any(|r| r.variety == VarietyId::new("basil")),
            "basil should still be offered"
        );
    }

    #[test]
    fn a_plant_not_yet_up_still_counts_as_a_coming_harvest() {
        // Something sown last week is very much coming, and planning around only what
        // has germinated would cluster everything behind it.
        let mut s = state();
        let planting = Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new("kale-lacinato"),
            t0(),
        );
        s.plantings.push(planting);

        assert_eq!(expected_harvests(&s).len(), 1, "an ungerminated plant counts");
    }

    #[test]
    fn a_harvest_already_past_is_not_something_to_plan_around() {
        let mut s = state();
        plant(&mut s, 0, "arugula", 200.0);
        assert!(
            expected_harvests(&s).iter().all(|d| *d > 0.0),
            "overdue harvests should not be future obstacles"
        );
    }

    #[test]
    fn the_tank_band_excludes_a_variety_that_cannot_share_the_solution() {
        // A tank is one solution. Suggesting a heavy feeder into a tank of greens
        // proposes a compromise that starves both.
        let mut s = state();
        plant(&mut s, 0, "red-cherry-tomato", 0.0);
        let band = tank_band(&s).expect("a band");

        let lean = s
            .varieties
            .iter()
            .find(|v| v.ec_target.is_some_and(|r| r.max < band.min))
            .map(|v| v.id.clone());

        if let Some(id) = lean {
            let ranked = suggest(&s, SlotId(1), 200);
            assert!(
                !ranked.iter().any(|r| r.variety == id),
                "{id:?} cannot share this tank and should not be offered"
            );
        }
    }

    #[test]
    fn an_empty_tank_constrains_nothing() {
        let s = state();
        assert_eq!(tank_band(&s), None);
        assert!(suggest(&s, SlotId(0), 5).len() == 5);
    }

    #[test]
    fn the_limit_is_honoured() {
        let s = state();
        assert_eq!(suggest(&s, SlotId(0), 3).len(), 3);
        assert!(suggest(&s, SlotId(0), 0).is_empty());
    }

    #[test]
    fn the_replant_hint_is_a_sentence_or_nothing() {
        let s = state();
        let hint = replacement_hint(&s, SlotId(0));
        assert!(hint.starts_with(" Consider "), "{hint:?}");
        assert!(hint.ends_with('.'), "{hint:?}");
        assert!(!hint.contains(".."), "double stop: {hint:?}");

        // An occupied slot has nothing to say, and must say it as an empty string
        // rather than a dangling fragment.
        let mut occupied = state();
        plant(&mut occupied, 0, "basil", 5.0);
        assert_eq!(replacement_hint(&occupied, SlotId(0)), "");
    }

    #[test]
    fn a_full_tower_still_answers_for_a_slot_that_frees_up() {
        // The realistic case: fifteen plants in, one pulled, what goes back?
        let mut s = state();
        for slot in 1..16u8 {
            plant(&mut s, slot, "basil", f64::from(slot));
        }
        let ranked = suggest(&s, SlotId(0), 3);
        assert!(!ranked.is_empty());
        assert!(ranked[0].clearance_days.is_finite());
    }

    #[test]
    fn planning_a_whole_tower_does_not_pick_the_same_thing_every_time() {
        // The bug this function exists to fix. Scoring each empty slot independently
        // gives every one of them the same answer, because the longest-maturing
        // variety is furthest from what is already growing — for all of them at once.
        // A plan reading "plant thirteen lemongrass" produces the exact pile-up it was
        // supposed to prevent.
        let s = state();
        let plan = plan_tower(&s, 1);
        assert_eq!(plan.len(), 16, "every slot is empty");

        let picks: Vec<&str> = plan
            .iter()
            .filter_map(|(_, ranked)| ranked.first().map(|r| r.name.as_str()))
            .collect();
        let distinct: std::collections::BTreeSet<&&str> = picks.iter().collect();
        assert!(
            distinct.len() > picks.len() / 2,
            "only {} distinct picks across {}: {picks:?}",
            distinct.len(),
            picks.len()
        );
    }

    #[test]
    fn a_planned_tower_spreads_its_harvests() {
        // The property that matters, stated directly: the gap between consecutive
        // harvests should be meaningful rather than everything landing together.
        let s = state();
        let mut days: Vec<f64> = plan_tower(&s, 1)
            .iter()
            .filter_map(|(_, ranked)| ranked.first().map(|r| r.harvest_in_days))
            .collect();
        days.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let span = days.last().unwrap() - days.first().unwrap();
        assert!(span > 40.0, "harvests span only {span:.0} days: {days:?}");

        // And no two land on the same day, which is the crude version of the same idea.
        assert!(
            days.windows(2).all(|w| (w[1] - w[0]).abs() > 0.5),
            "duplicate harvest days: {days:?}"
        );
    }

    #[test]
    fn planning_skips_slots_that_are_already_planted() {
        let mut s = state();
        plant(&mut s, 0, "basil", 5.0);
        let plan = plan_tower(&s, 1);
        assert_eq!(plan.len(), 15);
        assert!(!plan.iter().any(|(slot, _)| *slot == SlotId(0)));
    }

    #[test]
    fn coming_harvests_are_sorted_and_exclude_the_overdue() {
        let mut s = state();
        plant(&mut s, 0, "kale-lacinato", 5.0);
        plant(&mut s, 1, "arugula", 5.0);
        plant(&mut s, 2, "basil", 300.0); // long overdue

        let coming = coming_harvests(&s);
        assert!(coming.windows(2).all(|w| w[0].1 <= w[1].1), "not sorted: {coming:?}");
        assert!(coming.iter().all(|(_, d)| *d > 0.0));
    }

    #[test]
    fn geometry_beyond_the_studio_is_handled() {
        // The Home line is three columns of ten, and its zone map is unknown — the
        // planner must not assume the Studio's shape.
        let mut s = state();
        s.geometry = Geometry {
            columns: 3,
            rows_per_column: 10,
        };
        assert!(!suggest(&s, SlotId(29), 3).is_empty());
    }
}
