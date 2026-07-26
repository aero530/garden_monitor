//! What was done to the reservoir, and reconstructing its state from that.
//!
//! Sensors say how much water is in the tank right now. They cannot say when it was
//! last fed, conditioned, or scrubbed — and four of the rules ask exactly that. Those
//! answers only exist because someone recorded the action.
//!
//! Stored as an event log and folded forward rather than as a mutable row. Two
//! reasons: a mis-logged dose can be deleted and the state recomputes correctly, and
//! `gardyn-cli replay` can rebuild what the tank looked like on any past day, which is
//! what makes replaying history against a modified rule honest.

use crate::{Result, Store, StoreError, ts};
use gardyn_auth::UserId;
use gardyn_core::{GardenId, TankEvent, TankGeometry, TankState};
use gardyn_hal::Schedule;
use jiff::Timestamp;
use sqlx::Row;
use uuid::Uuid;

/// One recorded action.
#[derive(Debug, Clone, PartialEq)]
pub struct TankEventRecord {
    pub id: Uuid,
    pub event: TankEvent,
    pub actor: Option<UserId>,
    pub occurred_at: Timestamp,
}

impl Store {
    /// Record something the operator did to the tank.
    pub async fn record_tank_event(
        &self,
        garden: GardenId,
        event: TankEvent,
        actor: Option<UserId>,
        at: Timestamp,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let payload = serde_json::to_string(&event)
            .map_err(|e| StoreError::Corrupt(format!("tank event: {e}")))?;

        sqlx::query(
            "INSERT INTO tank_events (id, garden_id, payload, actor_id, occurred_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(id.to_string())
        .bind(garden.to_string())
        .bind(payload)
        .bind(actor.map(|a| a.to_string()))
        .bind(ts::encode(at))
        .execute(&self.db)
        .await?;
        Ok(id)
    }

    /// Every recorded action up to `until`, oldest first.
    pub async fn tank_events(
        &self,
        garden: GardenId,
        until: Timestamp,
    ) -> Result<Vec<TankEventRecord>> {
        let rows = sqlx::query(
            "SELECT id, payload, actor_id, occurred_at FROM tank_events
             WHERE garden_id = ?1 AND occurred_at <= ?2
             ORDER BY occurred_at, id",
        )
        .bind(garden.to_string())
        .bind(ts::encode(until))
        .fetch_all(&self.db)
        .await?;

        rows.iter()
            .map(|row| {
                let payload: String = row.try_get("payload")?;
                Ok(TankEventRecord {
                    id: Uuid::parse_str(&row.try_get::<String, _>("id")?)
                        .map_err(|e| StoreError::Corrupt(e.to_string()))?,
                    event: serde_json::from_str(&payload)
                        .map_err(|e| StoreError::Corrupt(format!("tank event: {e}")))?,
                    actor: row
                        .try_get::<Option<String>, _>("actor_id")?
                        .and_then(|a| a.parse().ok()),
                    occurred_at: ts::decode(&row.try_get::<String, _>("occurred_at")?)?,
                })
            })
            .collect()
    }

    /// Fold the log forward to reconstruct the tank as of `at`.
    ///
    /// Volume and consumption come from sensors afterwards where they exist; what this
    /// supplies is the part no sensor can — the *when* of each maintenance action.
    pub async fn tank_state_at(
        &self,
        garden: GardenId,
        geometry: &TankGeometry,
        at: Timestamp,
    ) -> Result<TankState> {
        let mut tank = TankState::new(geometry.capacity_l);
        for record in self.tank_events(garden, at).await? {
            record.event.apply(&mut tank, geometry, record.occurred_at);
        }
        Ok(tank)
    }

    /// Undo a mis-logged action.
    ///
    /// Deleting rather than reversing, because the log is folded forward on every
    /// read: remove the row and the state is simply as if it never happened. A
    /// compensating entry would leave two wrong timestamps behind instead of none.
    pub async fn delete_tank_event(&self, garden: GardenId, id: Uuid) -> Result<bool> {
        let result = sqlx::query("DELETE FROM tank_events WHERE garden_id = ?1 AND id = ?2")
            .bind(garden.to_string())
            .bind(id.to_string())
            .execute(&self.db)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests_support {
    pub use super::*;
    pub use gardyn_auth::EmailAddress;
    pub use gardyn_core::{DeviceModel, TaskKind, time::add_days};

    pub fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    pub async fn fixture() -> (Store, GardenId, UserId) {
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
        let garden = store
            .create_garden("Kitchen", DeviceModel::Studio2, "UTC", user.id, t0())
            .await
            .unwrap();
        (store, garden.id, user.id)
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;

    fn geometry() -> TankGeometry {
        TankGeometry::STUDIO_2
    }

    #[tokio::test]
    async fn a_fresh_garden_has_never_been_maintained() {
        let (store, garden, _) = fixture().await;
        let tank = store.tank_state_at(garden, &geometry(), t0()).await.unwrap();
        assert_eq!(tank.last_refresh, None);
        assert_eq!(tank.last_food_dose, None);
        assert_eq!(tank.last_deep_clean, None);
    }

    #[tokio::test]
    async fn a_recorded_action_moves_the_timestamp_the_rules_read() {
        // The whole point. Without this the maintenance rules fire on every tick
        // forever, because nothing they read ever changes.
        let (store, garden, user) = fixture().await;
        let fed = add_days(t0(), 3.0);
        store
            .record_tank_event(
                garden,
                TankEvent::FedToStrength { strength: 1.0 },
                Some(user),
                fed,
            )
            .await
            .unwrap();

        let tank = store
            .tank_state_at(garden, &geometry(), add_days(t0(), 5.0))
            .await
            .unwrap();
        assert_eq!(tank.last_food_dose, Some(fed));
        assert!((tank.days_since_food_dose(add_days(t0(), 5.0)) - 2.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn events_fold_forward_in_order() {
        let (store, garden, user) = fixture().await;
        // Drink the tank down, top it off twice, then feed it.
        for day in [1.0, 2.0] {
            store
                .record_tank_event(
                    garden,
                    TankEvent::TopOff { litres: 2.0 },
                    Some(user),
                    add_days(t0(), day),
                )
                .await
                .unwrap();
        }
        store
            .record_tank_event(
                garden,
                TankEvent::FedToStrength { strength: 1.0 },
                Some(user),
                add_days(t0(), 3.0),
            )
            .await
            .unwrap();

        let tank = store
            .tank_state_at(garden, &geometry(), add_days(t0(), 4.0))
            .await
            .unwrap();
        // Feeding resets the water-added counter, so the two top-offs before it are
        // accounted for rather than still pending a dose.
        assert_eq!(tank.litres_added_since_food_dose, 0.0);
        assert!((tank.estimated_strength() - 1.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn feeding_twice_does_not_compound_to_double_strength() {
        // `FedToStrength` rather than `FoodDose` exists for exactly this: the operator
        // who tops up the nutrients weekly should end at full strength, not at 300%.
        let (store, garden, user) = fixture().await;
        for day in [1.0, 8.0, 15.0] {
            store
                .record_tank_event(
                    garden,
                    TankEvent::FedToStrength { strength: 1.0 },
                    Some(user),
                    add_days(t0(), day),
                )
                .await
                .unwrap();
        }
        let tank = store
            .tank_state_at(garden, &geometry(), add_days(t0(), 20.0))
            .await
            .unwrap();
        assert!((tank.estimated_strength() - 1.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn state_is_reconstructed_as_of_a_past_moment() {
        // What makes replay honest: the tank has to look the way it looked then, not
        // the way it looks now.
        let (store, garden, user) = fixture().await;
        store
            .record_tank_event(garden, TankEvent::DeepClean, Some(user), add_days(t0(), 10.0))
            .await
            .unwrap();

        let before = store
            .tank_state_at(garden, &geometry(), add_days(t0(), 5.0))
            .await
            .unwrap();
        let after = store
            .tank_state_at(garden, &geometry(), add_days(t0(), 15.0))
            .await
            .unwrap();

        assert_eq!(before.last_deep_clean, None);
        assert_eq!(after.last_deep_clean, Some(add_days(t0(), 10.0)));
    }

    #[tokio::test]
    async fn a_mis_logged_event_can_be_removed_and_the_state_recovers() {
        let (store, garden, user) = fixture().await;
        let id = store
            .record_tank_event(garden, TankEvent::DeepClean, Some(user), t0())
            .await
            .unwrap();
        assert!(
            store
                .tank_state_at(garden, &geometry(), t0())
                .await
                .unwrap()
                .last_deep_clean
                .is_some()
        );

        assert!(store.delete_tank_event(garden, id).await.unwrap());
        assert_eq!(
            store
                .tank_state_at(garden, &geometry(), t0())
                .await
                .unwrap()
                .last_deep_clean,
            None
        );
    }

    #[tokio::test]
    async fn one_gardens_tank_log_is_invisible_to_another() {
        let (store, mine, user) = fixture().await;
        let theirs = store
            .create_garden("Theirs", DeviceModel::Studio2, "UTC", user, t0())
            .await
            .unwrap();
        store
            .record_tank_event(mine, TankEvent::DeepClean, Some(user), t0())
            .await
            .unwrap();

        assert_eq!(store.tank_events(theirs.id, t0()).await.unwrap().len(), 0);
        assert!(!store.delete_tank_event(theirs.id, Uuid::new_v4()).await.unwrap());
    }

    #[tokio::test]
    async fn every_tank_task_kind_has_an_event_to_record() {
        // If a task kind has no event, completing it silently does nothing and the
        // task returns on the next tick. Adding a kind should fail here first.
        let geometry = geometry();
        for kind in [
            TaskKind::AddPlantFood,
            TaskKind::AddConditioner,
            TaskKind::TankRefresh,
            TaskKind::DeepClean,
        ] {
            assert!(
                TankEvent::for_task(kind, &geometry).is_some(),
                "{kind} has no tank event"
            );
        }
        // Water is deliberately excluded: how much went in is not knowable from a
        // button press, and the level sensor measures it directly.
        assert_eq!(TankEvent::for_task(TaskKind::AddWater, &geometry), None);
        assert_eq!(TankEvent::for_task(TaskKind::Harvest, &geometry), None);
    }
}

// --- Schedule -------------------------------------------------------------------------

impl Store {
    /// The programme the agent should be running, if one has been set.
    ///
    /// `None` means the brain has no opinion, which the agent reads as "keep what you
    /// have". It never means "stop": a brain that has forgotten a garden must not be
    /// able to turn its lights off.
    pub async fn schedule(&self, garden: GardenId) -> Result<Option<Schedule>> {
        let row = sqlx::query("SELECT schedule FROM garden_schedule WHERE garden_id = ?1")
            .bind(garden.to_string())
            .fetch_optional(&self.db)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let text: String = row.try_get("schedule")?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| StoreError::Corrupt(format!("schedule: {e}")))
    }

    /// Set the programme. Refuses one the hardware should not be asked to run.
    ///
    /// Validated here as well as on the agent. The agent's check is the one that
    /// protects the pins; this one means a bad schedule is rejected where someone can
    /// see the error, rather than accepted and then silently ignored by every Pi.
    pub async fn set_schedule(
        &self,
        garden: GardenId,
        schedule: &Schedule,
        now: Timestamp,
    ) -> Result<()> {
        schedule
            .validate()
            .map_err(|e| StoreError::Corrupt(e.to_string()))?;
        let text = serde_json::to_string(schedule)
            .map_err(|e| StoreError::Corrupt(format!("schedule: {e}")))?;

        sqlx::query(
            "INSERT INTO garden_schedule (garden_id, schedule, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(garden_id) DO UPDATE SET
                schedule = excluded.schedule, updated_at = excluded.updated_at",
        )
        .bind(garden.to_string())
        .bind(text)
        .bind(ts::encode(now))
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Stop sending a schedule. The agent keeps running whatever it last received.
    pub async fn clear_schedule(&self, garden: GardenId) -> Result<()> {
        sqlx::query("DELETE FROM garden_schedule WHERE garden_id = ?1")
            .bind(garden.to_string())
            .execute(&self.db)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod schedule_tests {
    use super::tests_support::*;
    use gardyn_hal::Schedule;

    #[tokio::test]
    async fn a_garden_has_no_schedule_until_one_is_set() {
        let (store, garden, _) = fixture().await;
        assert_eq!(store.schedule(garden).await.unwrap(), None);

        store
            .set_schedule(garden, &Schedule::DEFAULT, t0())
            .await
            .unwrap();
        assert_eq!(store.schedule(garden).await.unwrap(), Some(Schedule::DEFAULT));
    }

    #[tokio::test]
    async fn setting_a_schedule_replaces_the_previous_one() {
        let (store, garden, _) = fixture().await;
        store
            .set_schedule(garden, &Schedule::DEFAULT, t0())
            .await
            .unwrap();
        let shorter = Schedule {
            light_hours: 12.0,
            ..Schedule::DEFAULT
        };
        store.set_schedule(garden, &shorter, t0()).await.unwrap();
        assert_eq!(store.schedule(garden).await.unwrap(), Some(shorter));
    }

    #[tokio::test]
    async fn a_schedule_the_hardware_should_not_run_is_refused_at_the_boundary() {
        // Rejected where a person can see the error, rather than accepted and then
        // silently ignored by every Pi that receives it.
        let (store, garden, _) = fixture().await;
        let greedy = Schedule {
            pump_duty: 1.0,
            ..Schedule::DEFAULT
        };
        assert!(store.set_schedule(garden, &greedy, t0()).await.is_err());
        assert_eq!(store.schedule(garden).await.unwrap(), None);
    }

    #[tokio::test]
    async fn clearing_means_no_opinion_rather_than_darkness() {
        let (store, garden, _) = fixture().await;
        store
            .set_schedule(garden, &Schedule::DEFAULT, t0())
            .await
            .unwrap();
        store.clear_schedule(garden).await.unwrap();
        assert_eq!(store.schedule(garden).await.unwrap(), None);
    }

    #[tokio::test]
    async fn one_gardens_schedule_is_invisible_to_another() {
        let (store, mine, user) = fixture().await;
        let theirs = store
            .create_garden(
                "Theirs",
                gardyn_core::DeviceModel::Studio2,
                "UTC",
                user,
                t0(),
            )
            .await
            .unwrap();
        store.set_schedule(mine, &Schedule::DEFAULT, t0()).await.unwrap();
        assert_eq!(store.schedule(theirs.id).await.unwrap(), None);
    }
}
