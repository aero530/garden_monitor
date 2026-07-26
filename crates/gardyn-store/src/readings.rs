//! Sensor readings from the edge agent.
//!
//! This is what turns a real garden from "no sensors reporting" into a live one. The
//! rules already know what to do with the readings; until this existed there was
//! simply nowhere for an agent to put them.

use crate::{Result, Store, ts};
use gardyn_core::{GardenId, SensorSnapshot};
use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

/// How long raw readings are kept before pruning.
///
/// At one sample a minute a single garden produces ~525k rows a year. Ninety days of
/// full resolution is enough to fit the growth and consumption curves; anything older
/// belongs in a rollup, not in this table.
pub const DEFAULT_RETENTION_DAYS: f64 = 90.0;

fn snapshot_from_row(row: &SqliteRow) -> Result<SensorSnapshot> {
    Ok(SensorSnapshot {
        at: ts::decode(&row.try_get::<String, _>("at")?)?,
        air_temp_c: row.try_get("air_temp_c")?,
        humidity_pct: row.try_get("humidity_pct")?,
        pcb_temp_c: row.try_get("pcb_temp_c")?,
        water_level_mm: row.try_get("water_level_mm")?,
        water_temp_c: row.try_get("water_temp_c")?,
        pump_current_ma: row.try_get("pump_current_ma")?,
        ec_ms_cm: row.try_get("ec_ms_cm")?,
        ph: row.try_get("ph")?,
    })
}

impl Store {
    /// Record a sample.
    ///
    /// Upserts on `(garden, at)` so an agent replaying a buffered backlog after a
    /// network outage cannot create duplicates.
    pub async fn record_reading(
        &self,
        garden: GardenId,
        sensors: &SensorSnapshot,
        agent_version: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO readings (garden_id, at, air_temp_c, humidity_pct, pcb_temp_c,
                water_level_mm, water_temp_c, pump_current_ma, ec_ms_cm, ph, agent_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(garden_id, at) DO UPDATE SET
                air_temp_c = excluded.air_temp_c,
                humidity_pct = excluded.humidity_pct,
                pcb_temp_c = excluded.pcb_temp_c,
                water_level_mm = excluded.water_level_mm,
                water_temp_c = excluded.water_temp_c,
                pump_current_ma = excluded.pump_current_ma,
                ec_ms_cm = excluded.ec_ms_cm,
                ph = excluded.ph,
                agent_version = excluded.agent_version",
        )
        .bind(garden.to_string())
        .bind(ts::encode(sensors.at))
        .bind(sensors.air_temp_c)
        .bind(sensors.humidity_pct)
        .bind(sensors.pcb_temp_c)
        .bind(sensors.water_level_mm)
        .bind(sensors.water_temp_c)
        .bind(sensors.pump_current_ma)
        .bind(sensors.ec_ms_cm)
        .bind(sensors.ph)
        .bind(agent_version)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn latest_reading(&self, garden: GardenId) -> Result<Option<SensorSnapshot>> {
        let row = sqlx::query("SELECT * FROM readings WHERE garden_id = ?1 ORDER BY at DESC LIMIT 1")
            .bind(garden.to_string())
            .fetch_optional(&self.db)
            .await?;
        row.as_ref().map(snapshot_from_row).transpose()
    }

    /// Readings since a point in time, oldest first. For charts and for fitting
    /// consumption rate.
    pub async fn readings_since(
        &self,
        garden: GardenId,
        since: Timestamp,
        limit: i64,
    ) -> Result<Vec<SensorSnapshot>> {
        let rows = sqlx::query(
            "SELECT * FROM readings WHERE garden_id = ?1 AND at >= ?2 ORDER BY at ASC LIMIT ?3",
        )
        .bind(garden.to_string())
        .bind(ts::encode(since))
        .bind(limit.clamp(1, 20_000))
        .fetch_all(&self.db)
        .await?;
        rows.iter().map(snapshot_from_row).collect()
    }

    pub async fn reading_count(&self, garden: GardenId) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM readings WHERE garden_id = ?1")
            .bind(garden.to_string())
            .fetch_one(&self.db)
            .await?;
        Ok(n)
    }

    pub async fn prune_readings(
        &self,
        garden: GardenId,
        keep_days: f64,
        now: Timestamp,
    ) -> Result<u64> {
        let cutoff = gardyn_core::time::add_days(now, -keep_days);
        let result = sqlx::query("DELETE FROM readings WHERE garden_id = ?1 AND at < ?2")
            .bind(garden.to_string())
            .bind(ts::encode(cutoff))
            .execute(&self.db)
            .await?;
        Ok(result.rows_affected())
    }

    /// Mean daily water consumption, fitted from the level history.
    ///
    /// Refills show up as the level going *up*, so only downward movement between
    /// consecutive samples is counted. Without that the estimate would be dragged
    /// toward zero every time someone topped the tank up.
    pub async fn fitted_consumption_lpd(
        &self,
        garden: GardenId,
        geometry: &gardyn_core::TankGeometry,
        since: Timestamp,
        now: Timestamp,
    ) -> Result<Option<f32>> {
        let readings = self.readings_since(garden, since, 20_000).await?;
        let levels: Vec<(Timestamp, f32)> = readings
            .iter()
            .filter_map(|r| r.water_level_mm.map(|mm| (r.at, geometry.volume_from_distance(mm))))
            .collect();

        if levels.len() < 2 {
            return Ok(None);
        }

        let mut drop_litres = 0.0f32;
        for pair in levels.windows(2) {
            let delta = pair[0].1 - pair[1].1;
            if delta > 0.0 {
                drop_litres += delta;
            }
        }

        let span_days = gardyn_core::time::days_between(levels[0].0, now);
        if span_days <= 0.0 {
            return Ok(None);
        }
        Ok(Some(drop_litres / span_days as f32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn snapshot(at: Timestamp, level_mm: Option<f32>) -> SensorSnapshot {
        let mut s = SensorSnapshot::empty(at);
        s.air_temp_c = Some(21.0);
        s.water_level_mm = level_mm;
        s
    }

    async fn fixture() -> (Store, GardenId) {
        let store = Store::in_memory().await.unwrap();
        let user = store
            .create_user(
                gardyn_auth::EmailAddress::parse("phil@example.com").unwrap(),
                "Phil",
                "a long enough password",
                t0(),
            )
            .await
            .unwrap();
        let garden = store
            .create_garden("Kitchen", gardyn_core::DeviceModel::Studio2, "UTC", user.id, t0())
            .await
            .unwrap();
        (store, garden.id)
    }

    #[tokio::test]
    async fn a_reading_round_trips_with_absent_probes_still_absent() {
        let (store, garden) = fixture().await;
        store
            .record_reading(garden, &snapshot(t0(), Some(150.0)), Some("0.1.0"))
            .await
            .unwrap();

        let latest = store.latest_reading(garden).await.unwrap().unwrap();
        assert_eq!(latest.air_temp_c, Some(21.0));
        assert_eq!(latest.water_level_mm, Some(150.0));
        // The distinction the capability model depends on.
        assert!(latest.ec_ms_cm.is_none());
        assert!(latest.ph.is_none());
    }

    #[tokio::test]
    async fn replaying_a_buffered_backlog_does_not_duplicate() {
        // The agent buffers to disk when the brain is unreachable and replays on
        // reconnect; the same sample can legitimately arrive twice.
        let (store, garden) = fixture().await;
        for _ in 0..3 {
            store
                .record_reading(garden, &snapshot(t0(), Some(150.0)), None)
                .await
                .unwrap();
        }
        assert_eq!(store.reading_count(garden).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn the_latest_reading_is_the_newest_one() {
        let (store, garden) = fixture().await;
        for minutes in [0.0, 30.0, 60.0] {
            let at = gardyn_core::time::add_days(t0(), minutes / (24.0 * 60.0));
            store
                .record_reading(garden, &snapshot(at, Some(150.0 - minutes as f32)), None)
                .await
                .unwrap();
        }
        let latest = store.latest_reading(garden).await.unwrap().unwrap();
        assert_eq!(latest.water_level_mm, Some(90.0));
    }

    #[tokio::test]
    async fn one_gardens_readings_never_appear_in_another() {
        let (store, kitchen) = fixture().await;
        let owner = store.members_of(kitchen).await.unwrap()[0].user.id;
        let office = store
            .create_garden("Office", gardyn_core::DeviceModel::Studio2, "UTC", owner, t0())
            .await
            .unwrap();

        store
            .record_reading(kitchen, &snapshot(t0(), Some(150.0)), None)
            .await
            .unwrap();

        assert_eq!(store.reading_count(kitchen).await.unwrap(), 1);
        assert_eq!(store.reading_count(office.id).await.unwrap(), 0);
        assert!(store.latest_reading(office.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn consumption_is_fitted_from_falling_levels() {
        let (store, garden) = fixture().await;
        let geometry = gardyn_core::TankGeometry::STUDIO_2;
        // Full at day 0, and 2 L lower by day 4.
        let full = geometry.full_distance_mm;
        let span = geometry.empty_distance_mm - geometry.full_distance_mm;
        let two_litres_lower = full + span * (2.0 / geometry.capacity_l);

        store
            .record_reading(garden, &snapshot(t0(), Some(full)), None)
            .await
            .unwrap();
        let day4 = gardyn_core::time::add_days(t0(), 4.0);
        store
            .record_reading(garden, &snapshot(day4, Some(two_litres_lower)), None)
            .await
            .unwrap();

        let rate = store
            .fitted_consumption_lpd(garden, &geometry, t0(), day4)
            .await
            .unwrap()
            .unwrap();
        assert!((rate - 0.5).abs() < 0.02, "expected ~0.5 L/day, got {rate}");
    }

    #[tokio::test]
    async fn a_refill_does_not_drag_the_consumption_estimate_down() {
        // The bug this guards: counting the upward step of a top-off as negative
        // consumption would report a garden that drinks nothing.
        let (store, garden) = fixture().await;
        let geometry = gardyn_core::TankGeometry::STUDIO_2;
        let full = geometry.full_distance_mm;
        let span = geometry.empty_distance_mm - geometry.full_distance_mm;
        let lower = full + span * (2.0 / geometry.capacity_l);

        // Down 2 L over four days, refilled, then down 2 L again.
        for (day, mm) in [(0.0, full), (4.0, lower), (4.1, full), (8.0, lower)] {
            let at = gardyn_core::time::add_days(t0(), day);
            store
                .record_reading(garden, &snapshot(at, Some(mm)), None)
                .await
                .unwrap();
        }

        let day8 = gardyn_core::time::add_days(t0(), 8.0);
        let rate = store
            .fitted_consumption_lpd(garden, &geometry, t0(), day8)
            .await
            .unwrap()
            .unwrap();
        assert!((rate - 0.5).abs() < 0.05, "expected ~0.5 L/day, got {rate}");
    }

    #[tokio::test]
    async fn a_single_reading_yields_no_estimate() {
        let (store, garden) = fixture().await;
        store
            .record_reading(garden, &snapshot(t0(), Some(150.0)), None)
            .await
            .unwrap();
        assert!(
            store
                .fitted_consumption_lpd(
                    garden,
                    &gardyn_core::TankGeometry::STUDIO_2,
                    t0(),
                    gardyn_core::time::add_days(t0(), 1.0)
                )
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn pruning_drops_old_samples_only() {
        let (store, garden) = fixture().await;
        store
            .record_reading(garden, &snapshot(t0(), Some(150.0)), None)
            .await
            .unwrap();
        let recent = gardyn_core::time::add_days(t0(), 100.0);
        store
            .record_reading(garden, &snapshot(recent, Some(140.0)), None)
            .await
            .unwrap();

        let now = gardyn_core::time::add_days(t0(), 101.0);
        let removed = store.prune_readings(garden, 90.0, now).await.unwrap();
        assert_eq!(removed, 1);
        assert_eq!(store.reading_count(garden).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn deleting_a_garden_takes_its_readings_with_it() {
        let (store, garden) = fixture().await;
        store
            .record_reading(garden, &snapshot(t0(), Some(150.0)), None)
            .await
            .unwrap();
        store.delete_garden(garden).await.unwrap();
        assert_eq!(store.reading_count(garden).await.unwrap(), 0);
    }
}
