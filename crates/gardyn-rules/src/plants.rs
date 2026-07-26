//! Per-plant work: thinning, pruning, pollination.

use crate::engine::{PRECEDENCE_FALLBACK, PRECEDENCE_MEASURED, Rule};
use gardyn_core::{
    Capability, DueWindow, GardenState, Planting, RuleId, Severity, Target, Task, TaskKind, Variety,
};

/// Cadence for pruning leggy or bushy varieties.
const PRUNE_DUE_DAYS: f64 = 21.0;
/// Canopy beyond this multiple of the harvest threshold needs cutting back regardless.
const PRUNE_CANOPY_MULTIPLE: f32 = 1.25;

fn plant_task(
    kind: TaskKind,
    planting: &Planting,
    severity: Severity,
    rationale: String,
    state: &GardenState,
    source: RuleId,
    window_days: f64,
) -> Task {
    Task::new(
        kind,
        Target::Planting(planting.id),
        severity,
        DueWindow::within_days(state.now, window_days),
        rationale,
        source,
    )
}

/// Thin seedlings during the documented weeks 2-6 window.
pub struct ThinByCalendarRule;

impl ThinByCalendarRule {
    pub const ID: RuleId = RuleId::from_static("thin-by-calendar");
}

impl Rule for ThinByCalendarRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::Thin]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_FALLBACK
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        state
            .planted()
            .filter(|(p, v)| p.needs_thinning(v, state.now))
            .map(|(p, v)| {
                let rationale = format!(
                    "{} is in its thinning window — reduce to {} per cube so the \
                     survivors are not competing",
                    v.name, v.thin_to
                );
                plant_task(
                    TaskKind::Thin,
                    p,
                    Severity::Advisory,
                    rationale,
                    state,
                    Self::ID,
                    10.0,
                )
            })
            .collect()
    }
}

/// Thin only when the camera can actually see too many seedlings.
pub struct ThinBySegmentationRule;

impl ThinBySegmentationRule {
    pub const ID: RuleId = RuleId::from_static("thin-by-segmentation");
}

impl Rule for ThinBySegmentationRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn requires(&self) -> &'static [Capability] {
        &[Capability::PlantSegmentation]
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::Thin]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_MEASURED
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        state
            .planted()
            .filter(|(p, v)| p.needs_thinning(v, state.now))
            .filter_map(|(planting, variety)| {
                let counted = state
                    .metrics_for(planting.slot)
                    .and_then(|m| m.plant_count);

                let Some(count) = counted else {
                    // Segmentation has nothing for this slot; keep the calendar behaviour.
                    let rationale = format!(
                        "{} is in its thinning window — reduce to {} per cube",
                        variety.name, variety.thin_to
                    );
                    return Some(plant_task(
                        TaskKind::Thin,
                        planting,
                        Severity::Advisory,
                        rationale,
                        state,
                        Self::ID,
                        10.0,
                    ));
                };

                // Germination is patchy; a cube that came up sparse needs no thinning
                // at all, which the calendar alone can never know.
                if count <= variety.thin_to {
                    return None;
                }

                let excess = count - variety.thin_to;
                let severity = if excess >= 3 {
                    Severity::Important
                } else {
                    Severity::Advisory
                };
                let rationale = format!(
                    "{count} seedlings detected in {} against a target of {}; remove {excess}",
                    variety.name, variety.thin_to
                );
                Some(plant_task(
                    TaskKind::Thin,
                    planting,
                    severity,
                    rationale,
                    state,
                    Self::ID,
                    10.0,
                ))
            })
            .collect()
    }
}

/// Cadence pruning for varieties that need shaping.
pub struct PrunePlantRule;

impl PrunePlantRule {
    pub const ID: RuleId = RuleId::from_static("prune-plant-cadence");

    fn due(planting: &Planting, variety: &Variety, state: &GardenState) -> bool {
        variety.needs_pruning
            && planting.stage(variety, state.now).is_producing()
            && planting.days_since_prune(state.now) >= PRUNE_DUE_DAYS
    }
}

impl Rule for PrunePlantRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::PrunePlant]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_FALLBACK
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        state
            .planted()
            .filter(|(p, v)| Self::due(p, v, state))
            .map(|(p, v)| {
                let since = p.days_since_prune(state.now);
                let rationale = if since.is_infinite() {
                    format!("{} has not been pruned yet", v.name)
                } else {
                    format!("{} last pruned {since:.0} days ago", v.name)
                };
                plant_task(
                    TaskKind::PrunePlant,
                    p,
                    Severity::Advisory,
                    rationale,
                    state,
                    Self::ID,
                    10.0,
                )
            })
            .collect()
    }
}

/// Prune when a plant is measurably shading its neighbours, not just on a timer.
pub struct PrunePlantByCanopyRule;

impl PrunePlantByCanopyRule {
    pub const ID: RuleId = RuleId::from_static("prune-plant-by-canopy");
}

impl Rule for PrunePlantByCanopyRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn requires(&self) -> &'static [Capability] {
        &[Capability::CanopyMetrics]
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::PrunePlant]
    }

    fn precedence(&self) -> u8 {
        PRECEDENCE_MEASURED
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        state
            .planted()
            .filter(|(_, v)| v.needs_pruning)
            .filter_map(|(planting, variety)| {
                let overgrown = state
                    .metrics_for(planting.slot)
                    .zip(variety.harvest_canopy_cm2)
                    .map(|(m, threshold)| {
                        (
                            m.canopy_area_cm2,
                            m.canopy_area_cm2 >= threshold * PRUNE_CANOPY_MULTIPLE,
                        )
                    });

                match overgrown {
                    Some((area, true)) => {
                        let rationale = format!(
                            "{} canopy at {area:.0} cm² is shading its neighbours; \
                             cut it back to keep the lower slots productive",
                            variety.name
                        );
                        Some(plant_task(
                            TaskKind::PrunePlant,
                            planting,
                            Severity::Important,
                            rationale,
                            state,
                            Self::ID,
                            5.0,
                        ))
                    }
                    // Measured and not overgrown: nothing to do, cadence notwithstanding.
                    Some((_, false)) => None,
                    // No measurement for this slot: keep the cadence behaviour.
                    None => Self::due_by_cadence(planting, variety, state),
                }
            })
            .collect()
    }
}

impl PrunePlantByCanopyRule {
    fn due_by_cadence(
        planting: &Planting,
        variety: &Variety,
        state: &GardenState,
    ) -> Option<Task> {
        PrunePlantRule::due(planting, variety, state).then(|| {
            plant_task(
                TaskKind::PrunePlant,
                planting,
                Severity::Advisory,
                format!("{} is due a routine prune", variety.name),
                state,
                Self::ID,
                10.0,
            )
        })
    }
}

/// Ask whether a cube has sprouted yet.
///
/// Without this the system has a silent dead end. Germination is the gate on every
/// other per-plant rule — thinning, harvest, root checks and replanting all measure
/// from it — so a planting whose sprout was never recorded sits at
/// [`Stage::Seeded`](gardyn_core::Stage::Seeded) forever and quietly generates no
/// advice at all. The operator has no way to notice that, because the symptom is an
/// absence.
pub struct GerminationCheckRule;

impl GerminationCheckRule {
    pub const ID: RuleId = RuleId::from_static("germination-check");
    const TAG: &'static str = "germination";
    /// Grace beyond the variety's expected sprout time before asking.
    const GRACE_DAYS: f64 = 2.0;
    /// Documented outside limit: everything should be up within 28 days.
    const FAILED_DAYS: f64 = 28.0;
}

impl Rule for GerminationCheckRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::Inspect]
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        state
            .planted()
            .filter(|(p, _)| p.germinated_at.is_none())
            .filter_map(|(planting, variety)| {
                let age = planting.age_days(state.now);
                let expected = f64::from(variety.germination_days);
                if age < expected + Self::GRACE_DAYS {
                    return None;
                }

                let (severity, rationale) = if age >= Self::FAILED_DAYS {
                    (
                        Severity::Important,
                        format!(
                            "{} was sown {age:.0} days ago and has not been recorded as \
                             sprouted; past {:.0} days it probably will not — consider \
                             replacing the cube",
                            variety.name,
                            Self::FAILED_DAYS
                        ),
                    )
                } else {
                    (
                        Severity::Advisory,
                        format!(
                            "{} should have sprouted by now ({age:.0} days, expected \
                             around {expected:.0}) — mark it sprouted so harvest timing \
                             can start",
                            variety.name
                        ),
                    )
                };

                Some(
                    plant_task(
                        TaskKind::Inspect,
                        planting,
                        severity,
                        rationale,
                        state,
                        Self::ID,
                        7.0,
                    )
                    .with_tag(Self::TAG),
                )
            })
            .collect()
    }
}

/// Fruiting plants indoors have no insects, so fruit set depends on the operator.
pub struct PollinationRule;

impl PollinationRule {
    pub const ID: RuleId = RuleId::from_static("pollinate");
}

impl Rule for PollinationRule {
    fn id(&self) -> RuleId {
        Self::ID
    }

    fn produces(&self) -> &'static [TaskKind] {
        &[TaskKind::Pollinate]
    }

    fn evaluate(&self, state: &GardenState) -> Vec<Task> {
        state
            .planted()
            .filter(|(p, v)| v.needs_pollination && p.stage(v, state.now).is_producing())
            .filter(|(p, _)| {
                // If segmentation can see flowers, trust it. Otherwise assume a
                // producing fruiting plant is worth a shake.
                state
                    .metrics_for(p.slot)
                    .and_then(|m| m.flowering)
                    .unwrap_or(true)
            })
            .map(|(p, v)| {
                plant_task(
                    TaskKind::Pollinate,
                    p,
                    Severity::Advisory,
                    format!(
                        "{} is flowering — shake the stem or brush the blossoms; \
                         indoors there is nothing else to do it",
                        v.name
                    ),
                    state,
                    Self::ID,
                    2.0,
                )
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

    fn garden(variety: &str, germinated_days_ago: f64) -> GardenState {
        let mut g = GardenState::new_studio_2(t0());
        let mut p = Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new(variety),
            add_days(t0(), -(germinated_days_ago + 8.0)),
        );
        p.germinated_at = Some(add_days(t0(), -germinated_days_ago));
        g.plantings.push(p);
        g
    }

    fn thin_engine() -> Engine {
        Engine::new(vec![
            Box::new(ThinByCalendarRule),
            Box::new(ThinBySegmentationRule),
        ])
    }

    fn prune_engine() -> Engine {
        Engine::new(vec![
            Box::new(PrunePlantRule),
            Box::new(PrunePlantByCanopyRule),
        ])
    }

    #[test]
    fn thinning_is_requested_inside_the_window() {
        // Arugula thins to 3.
        let tasks = thin_engine().evaluate(&garden("arugula", 20.0)).tasks;
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].rationale.contains("reduce to 3"));
    }

    #[test]
    fn thinning_is_not_requested_outside_the_window() {
        assert!(thin_engine().evaluate(&garden("arugula", 60.0)).tasks.is_empty());
    }

    #[test]
    fn segmentation_skips_thinning_when_germination_was_sparse() {
        let mut g = garden("arugula", 20.0);
        g.capabilities.insert(Capability::PlantSegmentation);
        let mut m = SlotMetrics::new(SlotId(0), t0(), 80.0);
        m.plant_count = Some(2); // target is 3; nothing to remove
        g.slot_metrics.insert(SlotId(0), m);

        assert!(
            thin_engine().evaluate(&g).tasks.is_empty(),
            "the calendar cannot know the cube came up sparse"
        );
    }

    #[test]
    fn segmentation_escalates_a_badly_overcrowded_cube() {
        let mut g = garden("arugula", 20.0);
        g.capabilities.insert(Capability::PlantSegmentation);
        let mut m = SlotMetrics::new(SlotId(0), t0(), 80.0);
        m.plant_count = Some(8);
        g.slot_metrics.insert(SlotId(0), m);

        let tasks = thin_engine().evaluate(&g).tasks;
        assert_eq!(tasks[0].severity, Severity::Important);
        assert!(tasks[0].rationale.contains("remove 5"));
    }

    #[test]
    fn pruning_follows_a_cadence_for_varieties_that_need_it() {
        // Basil needs pruning; mature at 28 days post-germination.
        let tasks = prune_engine().evaluate(&garden("basil", 70.0)).tasks;
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].rationale.contains("not been pruned yet"));
    }

    #[test]
    fn varieties_that_do_not_need_pruning_are_left_alone() {
        // Gardyn marks almost everything as needing pruning; Mini Cauliflower is one
        // of the few articles that explicitly says otherwise.
        assert!(
            prune_engine()
                .evaluate(&garden("mini-cauliflower", 70.0))
                .tasks
                .is_empty()
        );
    }

    #[test]
    fn a_measured_compact_plant_is_not_pruned_on_a_timer() {
        let mut g = garden("basil", 70.0);
        g.capabilities.insert(Capability::CanopyMetrics);
        g.slot_metrics
            .insert(SlotId(0), SlotMetrics::new(SlotId(0), t0(), 120.0));
        assert!(
            prune_engine().evaluate(&g).tasks.is_empty(),
            "small plant, no reason to cut it back"
        );
    }

    #[test]
    fn a_measured_overgrown_plant_is_escalated_for_shading() {
        let mut g = garden("basil", 70.0);
        g.capabilities.insert(Capability::CanopyMetrics);
        // Basil is compact, so the harvest threshold is 220 cm²; 1.25x is 275.
        g.slot_metrics
            .insert(SlotId(0), SlotMetrics::new(SlotId(0), t0(), 500.0));
        let tasks = prune_engine().evaluate(&g).tasks;
        assert_eq!(tasks[0].severity, Severity::Important);
        assert!(tasks[0].rationale.contains("shading its neighbours"));
    }

    #[test]
    fn fruiting_plants_are_asked_to_be_pollinated() {
        let g = garden("red-cherry-tomato", 70.0);
        let tasks = Engine::new(vec![Box::new(PollinationRule)]).evaluate(&g).tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].kind, TaskKind::Pollinate);
    }

    #[test]
    fn leafy_greens_are_never_asked_to_be_pollinated() {
        let g = garden("kale-lacinato", 70.0);
        assert!(Engine::new(vec![Box::new(PollinationRule)]).evaluate(&g).tasks.is_empty());
    }

    #[test]
    fn a_cube_that_has_not_sprouted_on_time_is_queried() {
        // Lacinato Kale sprouts in ~14 days; nothing is asked before then.
        let mut g = GardenState::new_studio_2(t0());
        let mut p = Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new("kale-lacinato"),
            add_days(t0(), -4.0),
        );
        p.germinated_at = None;
        g.plantings.push(p);

        let engine = Engine::new(vec![Box::new(GerminationCheckRule)]);
        assert!(engine.evaluate(&g).tasks.is_empty(), "too early to ask");

        g.plantings[0].planted_at = add_days(t0(), -20.0);
        let tasks = engine.evaluate(&g).tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].severity, Severity::Advisory);
        assert!(tasks[0].rationale.contains("mark it sprouted"));
    }

    #[test]
    fn a_cube_that_never_came_up_is_escalated() {
        let mut g = GardenState::new_studio_2(t0());
        g.plantings.push(Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new("kale-lacinato"),
            add_days(t0(), -35.0),
        ));
        let tasks = Engine::new(vec![Box::new(GerminationCheckRule)]).evaluate(&g).tasks;
        assert_eq!(tasks[0].severity, Severity::Important);
        assert!(tasks[0].rationale.contains("replacing the cube"));
    }

    #[test]
    fn recording_the_sprout_silences_the_query() {
        let mut g = GardenState::new_studio_2(t0());
        let mut p = Planting::new(
            PlantingId(1),
            SlotId(0),
            VarietyId::new("kale-lacinato"),
            add_days(t0(), -20.0),
        );
        p.germinated_at = Some(add_days(t0(), -14.0));
        g.plantings.push(p);
        assert!(
            Engine::new(vec![Box::new(GerminationCheckRule)])
                .evaluate(&g)
                .tasks
                .is_empty()
        );
    }

    #[test]
    fn segmentation_suppresses_pollination_when_nothing_is_flowering() {
        let mut g = garden("red-cherry-tomato", 70.0);
        let mut m = SlotMetrics::new(SlotId(0), t0(), 800.0);
        m.flowering = Some(false);
        g.slot_metrics.insert(SlotId(0), m);
        assert!(Engine::new(vec![Box::new(PollinationRule)]).evaluate(&g).tasks.is_empty());
    }
}
