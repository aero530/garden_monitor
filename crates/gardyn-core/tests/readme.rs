//! Every code example in README.md, executed.
//!
//! A README example that no longer compiles is worse than no example: it is a
//! confident, wrong answer to "how do I use this". Keeping them here costs one file
//! and means the build catches the drift.

use gardyn_core::{
    Capability, CapabilitySet, Severity, Target, TaskKey, TaskKind, VarietyBook, VarietyId,
};

#[test]
fn capabilities_are_runtime_state() {
    let stock = CapabilitySet::stock();
    let mine = stock.with(Capability::WaterTemperature);

    assert!(mine.contains(Capability::PumpCurrent));
    assert!(!mine.contains(Capability::Conductivity));
    assert_eq!(
        mine.missing(&[Capability::Conductivity]),
        vec![Capability::Conductivity]
    );
}

#[test]
fn a_task_is_identified_by_what_it_is() {
    let a = TaskKey::new(TaskKind::AddWater, Target::Garden);
    let b = TaskKey::new(TaskKind::AddWater, Target::Garden);
    assert_eq!(a, b);

    let roots = TaskKey::tagged(TaskKind::Inspect, Target::Garden, "roots");
    let algae = TaskKey::tagged(TaskKind::Inspect, Target::Garden, "algae");
    assert_ne!(roots, algae);
}

#[test]
fn severity_decides_whether_you_get_interrupted() {
    assert!(!Severity::Advisory.interrupts());
    assert!(Severity::Urgent.interrupts());
    assert_eq!(Severity::Critical.ntfy_priority(), 5);
}

#[test]
fn the_variety_book_carries_gardyns_own_words() {
    let book = VarietyBook::gardyn();
    let basil = book.get(&VarietyId::new("basil")).unwrap();

    assert_eq!(basil.germination_days, 13);
    assert!(basil.care.iter().any(|p| p.contains("bolting")));
}
