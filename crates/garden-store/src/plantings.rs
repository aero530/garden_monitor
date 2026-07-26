//! What is growing in which slot.
//!
//! This is the record the rule engine reads. Until it existed, every dashboard was
//! showing simulator output; with it, a garden with no sensors at all still gets
//! useful calendar-driven advice — thinning windows, harvest dates, root-check
//! cadence, end-of-life replanting — because all of those derive from the variety
//! book and a planting date rather than from hardware.

use crate::{Result, Store, ts};
use garden_auth::UserId;
use garden_core::{GardenId, Planting, PlantingId, SlotId, VarietyId};
use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlantingError {
    #[error("that slot already has something growing in it")]
    SlotOccupied,
    #[error("that slot does not exist on this device")]
    NoSuchSlot,
}

/// Something that happened to a plant, recorded against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlantingEvent {
    Germinated,
    Thinned,
    RootsChecked,
    Pruned,
    Harvested,
}

impl PlantingEvent {
    pub fn slug(self) -> &'static str {
        match self {
            PlantingEvent::Germinated => "germinated",
            PlantingEvent::Thinned => "thinned",
            PlantingEvent::RootsChecked => "roots-checked",
            PlantingEvent::Pruned => "pruned",
            PlantingEvent::Harvested => "harvested",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "germinated" => Some(PlantingEvent::Germinated),
            "thinned" => Some(PlantingEvent::Thinned),
            "roots-checked" => Some(PlantingEvent::RootsChecked),
            "pruned" => Some(PlantingEvent::Pruned),
            "harvested" => Some(PlantingEvent::Harvested),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PlantingEvent::Germinated => "sprouted",
            PlantingEvent::Thinned => "thinned",
            PlantingEvent::RootsChecked => "roots checked",
            PlantingEvent::Pruned => "pruned",
            PlantingEvent::Harvested => "harvested",
        }
    }

    /// The column this event stamps.
    fn column(self) -> &'static str {
        match self {
            PlantingEvent::Germinated => "germinated_at",
            PlantingEvent::Thinned => "thinned_at",
            PlantingEvent::RootsChecked => "last_root_check",
            PlantingEvent::Pruned => "last_prune",
            PlantingEvent::Harvested => "last_harvest",
        }
    }
}

fn planting_from_row(row: &SqliteRow) -> Result<Planting> {
    Ok(Planting {
        id: PlantingId(row.try_get::<i64, _>("id")? as u64),
        slot: SlotId(row.try_get::<i64, _>("slot")? as u8),
        variety: VarietyId::new(row.try_get::<String, _>("variety_id")?),
        planted_at: ts::decode(&row.try_get::<String, _>("planted_at")?)?,
        germinated_at: ts::decode_opt(row.try_get("germinated_at")?)?,
        thinned_at: ts::decode_opt(row.try_get("thinned_at")?)?,
        last_root_check: ts::decode_opt(row.try_get("last_root_check")?)?,
        last_prune: ts::decode_opt(row.try_get("last_prune")?)?,
        last_harvest: ts::decode_opt(row.try_get("last_harvest")?)?,
        harvest_count: row.try_get::<i64, _>("harvest_count")? as u32,
        removed_at: ts::decode_opt(row.try_get("removed_at")?)?,
    })
}

impl Store {
    /// Put a cube in a slot.
    ///
    /// Wrapped in a transaction because the id is allocated as `MAX(id) + 1` for the
    /// garden. Two people planting at once would otherwise race for the same number.
    pub async fn plant(
        &self,
        garden: GardenId,
        slot: SlotId,
        variety: &VarietyId,
        planted_at: Timestamp,
        slot_count: u8,
        by: Option<UserId>,
    ) -> Result<std::result::Result<Planting, PlantingError>> {
        if slot.0 >= slot_count {
            return Ok(Err(PlantingError::NoSuchSlot));
        }

        let mut tx = self.db.begin().await?;

        let (next_id,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(id), 0) + 1 FROM plantings WHERE garden_id = ?1",
        )
        .bind(garden.to_string())
        .fetch_one(&mut *tx)
        .await?;

        let result = sqlx::query(
            "INSERT INTO plantings (garden_id, id, slot, variety_id, planted_at, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(garden.to_string())
        .bind(next_id)
        .bind(i64::from(slot.0))
        .bind(&variety.0)
        .bind(ts::encode(planted_at))
        .bind(by.map(|u| u.to_string()))
        .execute(&mut *tx)
        .await;

        match result {
            Ok(_) => {
                tx.commit().await?;
                Ok(Ok(Planting {
                    id: PlantingId(next_id as u64),
                    slot,
                    variety: variety.clone(),
                    planted_at,
                    germinated_at: None,
                    thinned_at: None,
                    last_root_check: None,
                    last_prune: None,
                    last_harvest: None,
                    harvest_count: 0,
                    removed_at: None,
                }))
            }
            // The partial unique index caught a concurrent plant into the same slot.
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                Ok(Err(PlantingError::SlotOccupied))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Everything currently growing, ordered by slot.
    pub async fn active_plantings(&self, garden: GardenId) -> Result<Vec<Planting>> {
        let rows = sqlx::query(
            "SELECT * FROM plantings WHERE garden_id = ?1 AND removed_at IS NULL ORDER BY slot",
        )
        .bind(garden.to_string())
        .fetch_all(&self.db)
        .await?;
        rows.iter().map(planting_from_row).collect()
    }

    /// Everything ever grown here, most recently removed first.
    pub async fn planting_history(&self, garden: GardenId, limit: i64) -> Result<Vec<Planting>> {
        let rows = sqlx::query(
            "SELECT * FROM plantings WHERE garden_id = ?1 AND removed_at IS NOT NULL
             ORDER BY removed_at DESC LIMIT ?2",
        )
        .bind(garden.to_string())
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.db)
        .await?;
        rows.iter().map(planting_from_row).collect()
    }

    /// Every planting the garden has ever held, live and removed, oldest first.
    ///
    /// Distinct from [`Store::planting_history`], which returns only what has been
    /// pulled. Replay needs both: on any given past day some of these were growing and
    /// some had not been planted yet, and the caller filters by date.
    pub async fn all_plantings(&self, garden: GardenId) -> Result<Vec<Planting>> {
        let rows = sqlx::query(
            "SELECT * FROM plantings WHERE garden_id = ?1 ORDER BY planted_at, id",
        )
        .bind(garden.to_string())
        .fetch_all(&self.db)
        .await?;
        rows.iter().map(planting_from_row).collect()
    }

    pub async fn find_planting(
        &self,
        garden: GardenId,
        id: PlantingId,
    ) -> Result<Option<Planting>> {
        let row = sqlx::query("SELECT * FROM plantings WHERE garden_id = ?1 AND id = ?2")
            .bind(garden.to_string())
            .bind(id.0 as i64)
            .fetch_optional(&self.db)
            .await?;
        row.as_ref().map(planting_from_row).transpose()
    }

    pub async fn planting_in_slot(
        &self,
        garden: GardenId,
        slot: SlotId,
    ) -> Result<Option<Planting>> {
        let row = sqlx::query(
            "SELECT * FROM plantings
             WHERE garden_id = ?1 AND slot = ?2 AND removed_at IS NULL",
        )
        .bind(garden.to_string())
        .bind(i64::from(slot.0))
        .fetch_optional(&self.db)
        .await?;
        row.as_ref().map(planting_from_row).transpose()
    }

    /// Stamp an event against a planting.
    ///
    /// This is what closes the loop with the rule engine: a root-check cadence rule
    /// keeps firing until `last_root_check` moves, and the only thing that moves it is
    /// someone actually saying they did it.
    pub async fn record_planting_event(
        &self,
        garden: GardenId,
        id: PlantingId,
        event: PlantingEvent,
        at: Timestamp,
    ) -> Result<()> {
        // The column name comes from a closed enum, never from caller input.
        let sql = format!(
            "UPDATE plantings SET {} = ?1 WHERE garden_id = ?2 AND id = ?3 AND removed_at IS NULL",
            event.column()
        );
        sqlx::query(&sql)
            .bind(ts::encode(at))
            .bind(garden.to_string())
            .bind(id.0 as i64)
            .execute(&self.db)
            .await?;

        if event == PlantingEvent::Harvested {
            sqlx::query(
                "UPDATE plantings SET harvest_count = harvest_count + 1
                 WHERE garden_id = ?1 AND id = ?2 AND removed_at IS NULL",
            )
            .bind(garden.to_string())
            .bind(id.0 as i64)
            .execute(&self.db)
            .await?;
        }
        Ok(())
    }

    /// Pull a plant. The row stays for history and the slot frees up.
    pub async fn remove_planting(
        &self,
        garden: GardenId,
        id: PlantingId,
        at: Timestamp,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE plantings SET removed_at = ?1
             WHERE garden_id = ?2 AND id = ?3 AND removed_at IS NULL",
        )
        .bind(ts::encode(at))
        .bind(garden.to_string())
        .bind(id.0 as i64)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn set_planting_notes(
        &self,
        garden: GardenId,
        id: PlantingId,
        notes: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE plantings SET notes = ?1 WHERE garden_id = ?2 AND id = ?3")
            .bind(notes.trim())
            .bind(garden.to_string())
            .bind(id.0 as i64)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn planting_notes(
        &self,
        garden: GardenId,
        id: PlantingId,
    ) -> Result<Option<String>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT notes FROM plantings WHERE garden_id = ?1 AND id = ?2")
                .bind(garden.to_string())
                .bind(id.0 as i64)
                .fetch_optional(&self.db)
                .await?;
        Ok(row.and_then(|(n,)| n).filter(|n| !n.trim().is_empty()))
    }

    /// Convenience for seeding a garden, used when creating a simulated device.
    pub async fn plant_many(
        &self,
        garden: GardenId,
        entries: &[(SlotId, VarietyId, Timestamp)],
        slot_count: u8,
        by: Option<UserId>,
    ) -> Result<usize> {
        let mut planted = 0;
        for (slot, variety, at) in entries {
            if self
                .plant(garden, *slot, variety, *at, slot_count, by)
                .await?
                .is_ok()
            {
                planted += 1;
            }
        }
        Ok(planted)
    }
}

/// Map a completed task back onto the planting it was about.
///
/// Without this the rules would never stop asking: they re-emit from stored state, so
/// ticking "prune roots" has to move `last_root_check` or the same task reappears on
/// the next evaluation.
pub fn event_for_task(kind: garden_core::TaskKind) -> Option<PlantingEvent> {
    use garden_core::TaskKind;
    match kind {
        TaskKind::PruneRoots => Some(PlantingEvent::RootsChecked),
        TaskKind::PrunePlant => Some(PlantingEvent::Pruned),
        TaskKind::Harvest => Some(PlantingEvent::Harvested),
        TaskKind::Thin => Some(PlantingEvent::Thinned),
        // Water, food, conditioner, cleaning and inspection are about the tank or the
        // whole device, not about one plant.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use garden_core::TaskKind;

    #[test]
    fn plant_level_tasks_map_to_events() {
        assert_eq!(
            event_for_task(TaskKind::PruneRoots),
            Some(PlantingEvent::RootsChecked)
        );
        assert_eq!(
            event_for_task(TaskKind::Harvest),
            Some(PlantingEvent::Harvested)
        );
        assert_eq!(event_for_task(TaskKind::Thin), Some(PlantingEvent::Thinned));
    }

    #[test]
    fn garden_level_tasks_map_to_nothing() {
        for kind in [
            TaskKind::AddWater,
            TaskKind::AddPlantFood,
            TaskKind::TankRefresh,
            TaskKind::DeepClean,
            TaskKind::Inspect,
        ] {
            assert_eq!(event_for_task(kind), None, "{kind} should not touch a plant");
        }
    }

    #[test]
    fn events_round_trip_through_urls() {
        for event in [
            PlantingEvent::Germinated,
            PlantingEvent::Thinned,
            PlantingEvent::RootsChecked,
            PlantingEvent::Pruned,
            PlantingEvent::Harvested,
        ] {
            assert_eq!(PlantingEvent::parse(event.slug()), Some(event));
        }
        assert_eq!(PlantingEvent::parse("drop-table"), None);
    }
}
