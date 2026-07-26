//! Plantings, against a real database.

use gardyn_auth::{EmailAddress, UserId};
use gardyn_core::{DeviceModel, Garden, GardenId, PlantingId, SlotId, VarietyId};
use gardyn_store::Store;
use gardyn_store::plantings::{PlantingError, PlantingEvent};

const SLOTS: u8 = 16;

fn t0() -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_700_000_000).unwrap()
}

fn kale() -> VarietyId {
    VarietyId::new("kale-lacinato")
}

async fn fixture() -> (Store, Garden, Garden, UserId) {
    let store = Store::in_memory().await.unwrap();
    let user = store
        .create_user(
            EmailAddress::parse("phil@example.com").unwrap(),
            "Phil",
            "a long enough password",
            t0(),
        )
        .await
        .unwrap();
    let kitchen = store
        .create_garden("Kitchen", DeviceModel::Studio2, "UTC", user.id, t0())
        .await
        .unwrap();
    let office = store
        .create_garden("Office", DeviceModel::Studio2, "UTC", user.id, t0())
        .await
        .unwrap();
    (store, kitchen, office, user.id)
}

async fn plant(store: &Store, garden: GardenId, slot: u8) -> gardyn_core::Planting {
    store
        .plant(garden, SlotId(slot), &kale(), t0(), SLOTS, None)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn planting_a_slot_records_it() {
    let (store, kitchen, _, user) = fixture().await;
    let planting = store
        .plant(kitchen.id, SlotId(3), &kale(), t0(), SLOTS, Some(user))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(planting.slot, SlotId(3));
    assert_eq!(planting.variety, kale());
    assert!(planting.germinated_at.is_none());
    assert_eq!(planting.harvest_count, 0);

    let active = store.active_plantings(kitchen.id).await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, planting.id);
}

#[tokio::test]
async fn a_slot_holds_at_most_one_living_plant() {
    // Enforced by the partial unique index, not by a check-then-insert in Rust: two
    // people tending a shared garden can submit at the same moment.
    let (store, kitchen, _, _) = fixture().await;
    plant(&store, kitchen.id, 3).await;

    let second = store
        .plant(kitchen.id, SlotId(3), &VarietyId::new("arugula"), t0(), SLOTS, None)
        .await
        .unwrap();

    assert_eq!(second, Err(PlantingError::SlotOccupied));
    assert_eq!(store.active_plantings(kitchen.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn pulling_a_plant_frees_the_slot_and_keeps_the_history() {
    let (store, kitchen, _, _) = fixture().await;
    let first = plant(&store, kitchen.id, 3).await;

    store
        .remove_planting(kitchen.id, first.id, t0())
        .await
        .unwrap();
    assert!(store.active_plantings(kitchen.id).await.unwrap().is_empty());

    // The slot is free again...
    let second = store
        .plant(kitchen.id, SlotId(3), &VarietyId::new("arugula"), t0(), SLOTS, None)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(second.id, first.id);

    // ...and the old plant is still on the record.
    let history = store.planting_history(kitchen.id, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, first.id);
}

#[tokio::test]
async fn a_slot_beyond_the_device_is_refused() {
    let (store, kitchen, _, _) = fixture().await;
    let result = store
        .plant(kitchen.id, SlotId(99), &kale(), t0(), SLOTS, None)
        .await
        .unwrap();
    assert_eq!(result, Err(PlantingError::NoSuchSlot));
}

#[tokio::test]
async fn ids_are_allocated_per_garden() {
    let (store, kitchen, office, _) = fixture().await;
    let a = plant(&store, kitchen.id, 0).await;
    let b = plant(&store, kitchen.id, 1).await;
    let c = plant(&store, office.id, 0).await;

    assert_eq!(a.id, PlantingId(1));
    assert_eq!(b.id, PlantingId(2));
    // A fresh garden starts its own numbering, which is what makes ids readable in
    // task keys like "harvest:planting:1".
    assert_eq!(c.id, PlantingId(1));
}

#[tokio::test]
async fn ids_are_not_reused_after_a_removal() {
    // Reuse would let a stale task key resolve to a different plant.
    let (store, kitchen, _, _) = fixture().await;
    let first = plant(&store, kitchen.id, 0).await;
    store
        .remove_planting(kitchen.id, first.id, t0())
        .await
        .unwrap();
    let second = plant(&store, kitchen.id, 0).await;
    assert_ne!(second.id, first.id);
}

#[tokio::test]
async fn events_are_stamped_against_the_plant() {
    let (store, kitchen, _, _) = fixture().await;
    let planting = plant(&store, kitchen.id, 0).await;
    let later = gardyn_core::time::add_days(t0(), 7.0);

    for event in [
        PlantingEvent::Germinated,
        PlantingEvent::Thinned,
        PlantingEvent::RootsChecked,
        PlantingEvent::Pruned,
    ] {
        store
            .record_planting_event(kitchen.id, planting.id, event, later)
            .await
            .unwrap();
    }

    let reloaded = store
        .find_planting(kitchen.id, planting.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.germinated_at, Some(later));
    assert_eq!(reloaded.thinned_at, Some(later));
    assert_eq!(reloaded.last_root_check, Some(later));
    assert_eq!(reloaded.last_prune, Some(later));
}

#[tokio::test]
async fn harvesting_stamps_the_date_and_counts_up() {
    let (store, kitchen, _, _) = fixture().await;
    let planting = plant(&store, kitchen.id, 0).await;

    for n in 1..=3 {
        let at = gardyn_core::time::add_days(t0(), f64::from(n) * 10.0);
        store
            .record_planting_event(kitchen.id, planting.id, PlantingEvent::Harvested, at)
            .await
            .unwrap();
        let reloaded = store
            .find_planting(kitchen.id, planting.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.harvest_count, n as u32);
        assert_eq!(reloaded.last_harvest, Some(at));
    }
}

#[tokio::test]
async fn recording_a_root_check_stops_the_cadence_rule_repeating() {
    // The reason this table exists at all: the rule engine is stateless and
    // re-derives from stored state, so the only thing that quietens a cadence rule is
    // the stored date moving.
    let (store, kitchen, _, _) = fixture().await;
    let planted = gardyn_core::time::add_days(t0(), -60.0);
    let germinated = gardyn_core::time::add_days(t0(), -54.0);
    store
        .plant(kitchen.id, SlotId(0), &kale(), planted, SLOTS, None)
        .await
        .unwrap()
        .unwrap();
    store
        .record_planting_event(kitchen.id, PlantingId(1), PlantingEvent::Germinated, germinated)
        .await
        .unwrap();

    let build = |plantings: Vec<gardyn_core::Planting>| {
        let mut state = gardyn_core::GardenState::for_garden(kitchen.id, t0());
        state.capabilities = gardyn_core::CapabilitySet::empty();
        state.plantings = plantings;
        state
    };

    let before = build(store.active_plantings(kitchen.id).await.unwrap());
    let evaluation = gardyn_rules::default_engine().evaluate(&before);
    assert!(
        evaluation.has(gardyn_core::TaskKind::PruneRoots),
        "never-checked roots should be asked about"
    );

    store
        .record_planting_event(kitchen.id, PlantingId(1), PlantingEvent::RootsChecked, t0())
        .await
        .unwrap();

    let after = build(store.active_plantings(kitchen.id).await.unwrap());
    let evaluation = gardyn_rules::default_engine().evaluate(&after);
    assert!(
        !evaluation.has(gardyn_core::TaskKind::PruneRoots),
        "recording the check should have quietened the rule"
    );
}

#[tokio::test]
async fn one_gardens_plantings_never_appear_in_another() {
    let (store, kitchen, office, _) = fixture().await;
    plant(&store, kitchen.id, 0).await;

    assert_eq!(store.active_plantings(kitchen.id).await.unwrap().len(), 1);
    assert!(store.active_plantings(office.id).await.unwrap().is_empty());
    // The same slot is independently available in the other garden.
    assert!(
        store
            .plant(office.id, SlotId(0), &kale(), t0(), SLOTS, None)
            .await
            .unwrap()
            .is_ok()
    );
}

#[tokio::test]
async fn a_planting_id_from_another_garden_does_not_resolve() {
    let (store, kitchen, office, _) = fixture().await;
    let planting = plant(&store, kitchen.id, 0).await;
    assert!(
        store
            .find_planting(office.id, planting.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn events_cannot_be_aimed_at_a_plant_in_another_garden() {
    let (store, kitchen, office, _) = fixture().await;
    let planting = plant(&store, kitchen.id, 0).await;

    // Same numeric id, wrong garden: must be a no-op, not a cross-tenant write.
    store
        .record_planting_event(office.id, planting.id, PlantingEvent::Harvested, t0())
        .await
        .unwrap();

    let untouched = store
        .find_planting(kitchen.id, planting.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(untouched.harvest_count, 0);
}

#[tokio::test]
async fn events_do_not_apply_to_a_pulled_plant() {
    let (store, kitchen, _, _) = fixture().await;
    let planting = plant(&store, kitchen.id, 0).await;
    store
        .remove_planting(kitchen.id, planting.id, t0())
        .await
        .unwrap();

    store
        .record_planting_event(kitchen.id, planting.id, PlantingEvent::Harvested, t0())
        .await
        .unwrap();

    let reloaded = store
        .find_planting(kitchen.id, planting.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.harvest_count, 0, "a pulled plant was still harvested");
}

#[tokio::test]
async fn deleting_a_garden_takes_its_plantings_with_it() {
    let (store, kitchen, _, _) = fixture().await;
    plant(&store, kitchen.id, 0).await;
    store.delete_garden(kitchen.id).await.unwrap();
    assert!(store.active_plantings(kitchen.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn seeding_a_garden_fills_several_slots_at_once() {
    let (store, kitchen, _, user) = fixture().await;
    let entries = vec![
        (SlotId(0), kale(), t0()),
        (SlotId(1), VarietyId::new("arugula"), t0()),
        (SlotId(2), VarietyId::new("basil"), t0()),
    ];
    let planted = store
        .plant_many(kitchen.id, &entries, SLOTS, Some(user))
        .await
        .unwrap();

    assert_eq!(planted, 3);
    assert_eq!(store.active_plantings(kitchen.id).await.unwrap().len(), 3);
}

#[tokio::test]
async fn seeding_skips_slots_that_are_already_taken() {
    let (store, kitchen, _, _) = fixture().await;
    plant(&store, kitchen.id, 0).await;

    let entries = vec![(SlotId(0), kale(), t0()), (SlotId(1), kale(), t0())];
    let planted = store.plant_many(kitchen.id, &entries, SLOTS, None).await.unwrap();

    assert_eq!(planted, 1, "the occupied slot should have been skipped");
    assert_eq!(store.active_plantings(kitchen.id).await.unwrap().len(), 2);
}

#[tokio::test]
async fn notes_round_trip_and_blank_notes_read_as_absent() {
    let (store, kitchen, _, _) = fixture().await;
    let planting = plant(&store, kitchen.id, 0).await;

    assert_eq!(store.planting_notes(kitchen.id, planting.id).await.unwrap(), None);
    store
        .set_planting_notes(kitchen.id, planting.id, "  leggy, needs more light  ")
        .await
        .unwrap();
    assert_eq!(
        store.planting_notes(kitchen.id, planting.id).await.unwrap(),
        Some("leggy, needs more light".to_string())
    );

    store.set_planting_notes(kitchen.id, planting.id, "   ").await.unwrap();
    assert_eq!(store.planting_notes(kitchen.id, planting.id).await.unwrap(), None);
}
