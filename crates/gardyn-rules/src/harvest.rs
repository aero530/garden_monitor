//! Harvest timing.
//!
//! The variety book gives an expected date. Canopy measurement gives the actual plant.
//! Real growth runs ahead or behind the book depending on slot position, light, and
//! solution strength, so when vision is enabled it should lead.

use crate::engine::{PRECEDENCE_FALLBACK, PRECEDENCE_MEASURED, Rule};
use gardyn_core::{
    Capability, DueWindow, GardenState, Planting, RuleId, Severity, Stage, Target, Task, TaskKind,
    Variety,
};

/// Canopy beyond this multiple of the harvest threshold is crowding its neighbours.
const OVERGROWN_MULTIPLE: f32 = 1.3;
/// Days past the expected date before a missed harvest is escalated.
const OVERDUE_DAYS: f64 = 7.0;

/// Plantings the variety book considers ready to yield.
fn scheduled(state: &GardenState) -> impl Iterator<Item = (&Planting, &Variety)> {
    state
        .planted()
        .filter(|(p, v)| p.stage(v, state.now).is_producing())
}

/// Plantings the camera is allowed to judge.
///
/// Deliberately looser than [`scheduled`]. "Producing" there is derived from the
/// calendar, so gating the measured rule behind it would make harvesting early
/// impossible — which is most of the reason to measure at all. A plant that reaches
/// its target size a week ahead of the book should be picked a week ahead of the book.
fn measurable(state: &GardenState) -> impl Iterator<Item = (&Planting, &Variety)> {
    state.planted().filter(|(p, v)| {
        !matches!(
            p.stage(v, state.now),
            Stage::Seeded | Stage::Seedling | Stage::Spent
        )
    })
}

fn task(
    planting: &Planting,
    severity: Severity,
    rationale: String,
    state: &GardenState,
    source: RuleId,
) -> Task {
    Task::new(
        TaskKind::Harvest,
        Target::Planting(planting.id),
        severity,
        DueWindow::within_days(state.now, 4.0),
        rationale,
        source,
    )
}

/// Harvest on the schedule in the variety book.
pub struct HarvestByCalendarRule;

impl HarvestByCalendarRule {
    pub const ID: RuleId = RuleId::from_static("harvest-by-calendar");

    fn assess(planting: &Planting, variety: &Variety, state: &GardenState) -> Option<(Severity, String)> {
        let remaining = planting.days_until_harvest(variety, state.now)?;
        if remaining > 0.0 {
            return None;
        }
        let overdue = -remaining;
        let severity = if overdue >= OVERDUE_DAYS {
            Severity::Important
        } else {
            Severity::Advisory
        };
        let rationale = if overdue < 1.0 {
            format!("{} is due for harvest", variety.name)
        } else {
            format!(
                "{} has been ready for {overdue:.0} days; leaving it reduces the next flush",
                variety.name
            )
        };
        Some((severity, rationale))
    }
}

impl Rule for HarvestByCalendarRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::Harvest]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_FALLBACK
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        scheduled(state)
            .filter_map(|(p, v)| {
                Self::assess(p, v, state).map(|(sev, why)| task(p, sev, why, state, Self::ID))
            })
            .collect()
    }
}

/// Harvest on measured canopy, falling back to the calendar per slot.
pub struct HarvestByCanopyRule;

impl HarvestByCanopyRule {
    pub const ID: RuleId = RuleId::from_static("harvest-by-canopy");
}

impl Rule for HarvestByCanopyRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn requires(&self) -> &'static [Capability] {
        &[Capability::CanopyMetrics]
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::Harvest]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_MEASURED
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        measurable(state)
            .filter_map(|(planting, variety)| {
                let measured = state
                    .metrics_for(planting.slot)
                    .zip(variety.harvest_canopy_cm2);

                // No usable measurement for this slot — occluded, or vision has not
                // caught up. Defer to the book rather than going silent.
                let Some((metrics, threshold)) = measured else {
                    return HarvestByCalendarRule::assess(planting, variety, state)
                        .map(|(sev, why)| task(planting, sev, why, state, Self::ID));
                };

                let area = metrics.canopy_area_cm2;
                if area < threshold {
                    return None;
                }

                let overgrown = area >= threshold * OVERGROWN_MULTIPLE;
                let severity = if overgrown {
                    Severity::Important
                } else {
                    Severity::Advisory
                };
                let calendar_note = match planting.days_until_harvest(variety, state.now) {
                    Some(d) if d > 1.0 => format!(", {d:.0} days ahead of the book"),
                    Some(d) if d < -1.0 => format!(", {:.0} days behind the book", -d),
                    _ => String::new(),
                };
                let crowding = if overgrown {
                    " and is crowding its neighbours"
                } else {
                    ""
                };
                let rationale = format!(
                    "{} canopy at {area:.0} cm² against a {threshold:.0} cm² harvest \
                     threshold{crowding}{calendar_note}",
                    variety.name
                );
                Some(task(planting, severity, rationale, state, Self::ID))
            })
            .collect()
    }
}

/// Plantings past their productive life should be pulled and the slot replanted.
pub struct ReplantRule;

impl ReplantRule {
    pub const ID: RuleId = RuleId::from_static("replant");
}

impl Rule for ReplantRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::Replant]
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        state
            .planted()
            .filter_map(|(planting, variety)| {
                let (severity, rationale) = match planting.stage(variety, state.now) {
                    Stage::Spent => (
                        Severity::Advisory,
                        format!(
                            "{} has passed its {}-day productive life; the slot will \
                             yield more with a fresh cube",
                            variety.name, variety.productive_life_days
                        ),
                    ),
                    Stage::Declining => (
                        Severity::Info,
                        format!(
                            "{} is winding down — worth starting a replacement so the \
                             slot is not idle",
                            variety.name
                        ),
                    ),
                    _ => return None,
                };
                Some(Task::new(
                    TaskKind::Replant,
                    Target::Planting(planting.id),
                    severity,
                    DueWindow::within_days(state.now, 14.0),
                    rationale,
                    Self::ID,
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use gardyn_core::{
        PlantingId, SlotId, SlotMetrics, Timestamp, VarietyId, time::add_days,
    };

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    /// Kale: germinates in 6 days, first harvest 35 days after germination.
    fn garden(germinated_days_ago: f64, harvest_count: u32) -> GardenState {
        let mut g = GardenState::new_studio_2(t0());
        let mut p = Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new("kale-lacinato"),
            add_days(t0(), -(germinated_days_ago + 6.0)),
        );
        p.germinated_at = Some(add_days(t0(), -germinated_days_ago));
        p.harvest_count = harvest_count;
        g.plantings.push(p);
        g
    }

    fn with_canopy(mut g: GardenState, area_cm2: f32) -> GardenState {
        g.capabilities.insert(Capability::CanopyMetrics);
        g.slot_metrics
            .insert(SlotId(0), SlotMetrics::new(SlotId(0), t0(), area_cm2));
        g
    }

    fn engine() -> Engine {
        Engine::new(vec![
            Box::new(HarvestByCalendarRule),
            Box::new(HarvestByCanopyRule),
        ])
    }

    #[test]
    fn an_immature_plant_is_not_harvested() {
        assert!(engine().evaluate(&garden(20.0, 0)).tasks.is_empty());
    }

    #[test]
    fn the_calendar_fires_on_the_expected_date() {
        let tasks = engine().evaluate(&garden(36.0, 0)).tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].severity, Severity::Advisory);
    }

    #[test]
    fn a_long_missed_harvest_escalates() {
        let tasks = engine().evaluate(&garden(45.0, 0)).tasks;
        assert_eq!(tasks[0].severity, Severity::Important);
        assert!(tasks[0].rationale.contains("reduces the next flush"));
    }

    #[test]
    fn each_harvest_pushes_the_next_one_out() {
        // Day 40 with one harvest taken: next is due at day 45, so nothing yet.
        assert!(engine().evaluate(&garden(40.0, 1)).tasks.is_empty());
        assert_eq!(engine().evaluate(&garden(46.0, 1)).tasks.len(), 1);
    }

    #[test]
    fn a_fast_growing_plant_is_harvested_ahead_of_the_book() {
        // Day 30: the book says wait until 35, but the canopy is already there.
        let g = with_canopy(garden(30.0, 0), 600.0);
        let tasks = engine().evaluate(&g).tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].source, HarvestByCanopyRule::ID);
        assert!(tasks[0].rationale.contains("ahead of the book"), "{}", tasks[0].rationale);
    }

    #[test]
    fn a_slow_growing_plant_is_left_to_keep_growing() {
        // Day 40: the book says harvest, but it is only half the target size.
        let g = with_canopy(garden(40.0, 0), 260.0);
        assert!(
            engine().evaluate(&g).tasks.is_empty(),
            "measurement should override an optimistic calendar"
        );
    }

    #[test]
    fn an_overgrown_plant_is_escalated_for_crowding() {
        let g = with_canopy(garden(38.0, 0), 800.0); // >1.3x the 520 threshold
        let tasks = engine().evaluate(&g).tasks;
        assert_eq!(tasks[0].severity, Severity::Important);
        assert!(tasks[0].rationale.contains("crowding"));
    }

    #[test]
    fn a_slot_with_no_metrics_falls_back_to_the_calendar() {
        // Vision is on, but this slot has no reading — occluded, or not yet processed.
        let mut g = garden(40.0, 0);
        g.capabilities.insert(Capability::CanopyMetrics);
        let eval = engine().evaluate(&g);
        assert_eq!(eval.tasks.len(), 1, "must not go silent");
        assert_eq!(eval.tasks[0].source, HarvestByCanopyRule::ID);
    }

    #[test]
    fn enabling_vision_suppresses_the_calendar_rule() {
        let g = with_canopy(garden(40.0, 0), 600.0);
        assert!(engine().evaluate(&g).was_suppressed("harvest-by-calendar"));
    }

    #[test]
    fn spent_plantings_are_flagged_for_replacement() {
        let g = garden(160.0, 5); // kale productive life is 150 days
        let tasks = Engine::new(vec![Box::new(ReplantRule)]).evaluate(&g).tasks;
        assert_eq!(tasks[0].kind, TaskKind::Replant);
        assert_eq!(tasks[0].severity, Severity::Advisory);
    }

    #[test]
    fn declining_plantings_get_a_quiet_heads_up_not_an_alert() {
        let g = garden(130.0, 5); // decline starts at 120
        let tasks = Engine::new(vec![Box::new(ReplantRule)]).evaluate(&g).tasks;
        assert_eq!(tasks[0].severity, Severity::Info);
        assert!(!tasks[0].severity.interrupts());
    }
}
