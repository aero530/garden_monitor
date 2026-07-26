//! Storing what the camera measured, and where to look.
//!
//! Two things live here. The **ROI map** is calibration: one document per garden
//! saying which pixels are which slot. Its presence is also the on/off switch for
//! vision, because there is no way to measure slot 7 without knowing where slot 7 is —
//! "not calibrated" and "no canopy metrics" are the same fact, not two settings that
//! can disagree.
//!
//! The **metrics** are the output: per-slot rows keyed by the frame they came from, so
//! a re-analysis replaces its own results rather than leaving two opinions side by
//! side, and deleting a photograph takes its measurements with it.

use crate::{Result, Store, ts};
use gardyn_core::{AlgaeReading, GardenId, SlotId, SlotMetrics};
use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

/// How long per-slot metrics are kept.
///
/// Longer than raw sensor readings: one row per slot per frame is a few thousand a
/// year, and a full season of canopy history is what makes a growth curve worth
/// looking at.
pub const DEFAULT_RETENTION_DAYS: f64 = 400.0;

fn metrics_from_row(row: &SqliteRow) -> Result<SlotMetrics> {
    Ok(SlotMetrics {
        slot: SlotId(row.try_get::<i64, _>("slot")? as u8),
        at: ts::decode(&row.try_get::<String, _>("at")?)?,
        canopy_area_cm2: row.try_get::<f64, _>("canopy_area_cm2")? as f32,
        green_fraction: row.try_get::<f64, _>("green_fraction")? as f32,
        yellowing_index: row.try_get::<f64, _>("yellowing_index")? as f32,
        growth_rate_cm2_per_day: row.try_get::<f64, _>("growth_rate")? as f32,
        plant_count: row.try_get::<Option<i64>, _>("plant_count")?.map(|c| c as u8),
        flowering: row.try_get::<Option<i64>, _>("flowering")?.map(|f| f != 0),
        diagnosis: row.try_get("diagnosis")?,
    })
}

impl Store {
    // --- Calibration ---------------------------------------------------------------

    /// Store a garden's ROI map, as the JSON document `gardyn-vision` serialises.
    ///
    /// Kept as opaque text rather than exploded into columns. The map is written by
    /// one tool and read by one crate, and normalising it would mean a migration every
    /// time a calibration field is added — for data nothing else ever queries.
    pub async fn save_roi_map(
        &self,
        garden: GardenId,
        roi_map: &str,
        now: Timestamp,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO vision_config (garden_id, roi_map, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(garden_id) DO UPDATE SET
                roi_map = excluded.roi_map, updated_at = excluded.updated_at",
        )
        .bind(garden.to_string())
        .bind(roi_map)
        .bind(ts::encode(now))
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn roi_map(&self, garden: GardenId) -> Result<Option<String>> {
        let row = sqlx::query("SELECT roi_map FROM vision_config WHERE garden_id = ?1")
            .bind(garden.to_string())
            .fetch_optional(&self.db)
            .await?;
        row.map(|r| r.try_get("roi_map")).transpose().map_err(Into::into)
    }

    /// Turn vision off by forgetting where the slots are.
    ///
    /// Existing metrics are left alone: they were true when they were measured, and a
    /// recalibration should not erase last month's growth curve.
    pub async fn clear_roi_map(&self, garden: GardenId) -> Result<()> {
        sqlx::query("DELETE FROM vision_config WHERE garden_id = ?1")
            .bind(garden.to_string())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    // --- Measurements --------------------------------------------------------------

    /// Record everything one frame produced, replacing any earlier analysis of it.
    pub async fn record_slot_metrics(
        &self,
        garden: GardenId,
        frame: Uuid,
        metrics: &[SlotMetrics],
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        for m in metrics {
            sqlx::query(
                "INSERT INTO slot_metrics (garden_id, frame_id, slot, at, canopy_area_cm2,
                    green_fraction, yellowing_index, growth_rate, plant_count, flowering,
                    diagnosis)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(frame_id, slot) DO UPDATE SET
                    canopy_area_cm2 = excluded.canopy_area_cm2,
                    green_fraction  = excluded.green_fraction,
                    yellowing_index = excluded.yellowing_index,
                    growth_rate     = excluded.growth_rate,
                    plant_count     = excluded.plant_count,
                    flowering       = excluded.flowering,
                    diagnosis       = excluded.diagnosis",
            )
            .bind(garden.to_string())
            .bind(frame.to_string())
            .bind(i64::from(m.slot.0))
            .bind(ts::encode(m.at))
            .bind(f64::from(m.canopy_area_cm2))
            .bind(f64::from(m.green_fraction))
            .bind(f64::from(m.yellowing_index))
            .bind(f64::from(m.growth_rate_cm2_per_day))
            .bind(m.plant_count.map(i64::from))
            .bind(m.flowering.map(i64::from))
            .bind(m.diagnosis.as_deref())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn record_algae(
        &self,
        garden: GardenId,
        frame: Uuid,
        reading: AlgaeReading,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO algae_readings (garden_id, frame_id, at, coverage)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(frame_id) DO UPDATE SET
                at = excluded.at, coverage = excluded.coverage",
        )
        .bind(garden.to_string())
        .bind(frame.to_string())
        .bind(ts::encode(reading.at))
        .bind(f64::from(reading.coverage))
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// The most recent measurement for each slot, for the rule engine.
    ///
    /// Slots are taken independently rather than from one frame, because a frame where
    /// slot 3 was in shadow still measured the other fifteen. Requiring them to agree
    /// on a frame would throw away good readings to preserve a tidiness nothing needs.
    pub async fn latest_slot_metrics(&self, garden: GardenId) -> Result<Vec<SlotMetrics>> {
        let rows = sqlx::query(
            "SELECT m.* FROM slot_metrics m
             JOIN (SELECT slot, MAX(at) AS at FROM slot_metrics
                   WHERE garden_id = ?1 GROUP BY slot) latest
               ON m.slot = latest.slot AND m.at = latest.at
             WHERE m.garden_id = ?1
             ORDER BY m.slot",
        )
        .bind(garden.to_string())
        .fetch_all(&self.db)
        .await?;
        rows.iter().map(metrics_from_row).collect()
    }

    /// Canopy history for one slot, oldest first, for fitting a growth rate.
    pub async fn canopy_history(
        &self,
        garden: GardenId,
        slot: SlotId,
        since: Timestamp,
    ) -> Result<Vec<(Timestamp, f32)>> {
        let rows = sqlx::query(
            "SELECT at, canopy_area_cm2 FROM slot_metrics
             WHERE garden_id = ?1 AND slot = ?2 AND at >= ?3
             ORDER BY at",
        )
        .bind(garden.to_string())
        .bind(i64::from(slot.0))
        .bind(ts::encode(since))
        .fetch_all(&self.db)
        .await?;

        rows.iter()
            .map(|r| {
                Ok((
                    ts::decode(&r.try_get::<String, _>("at")?)?,
                    r.try_get::<f64, _>("canopy_area_cm2")? as f32,
                ))
            })
            .collect()
    }

    pub async fn latest_algae(&self, garden: GardenId) -> Result<Option<AlgaeReading>> {
        let row = sqlx::query(
            "SELECT at, coverage FROM algae_readings
             WHERE garden_id = ?1 ORDER BY at DESC LIMIT 1",
        )
        .bind(garden.to_string())
        .fetch_optional(&self.db)
        .await?;

        row.map(|r| {
            Ok(AlgaeReading {
                at: ts::decode(&r.try_get::<String, _>("at")?)?,
                coverage: r.try_get::<f64, _>("coverage")? as f32,
            })
        })
        .transpose()
    }

    pub async fn slot_metrics_count(&self, garden: GardenId) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM slot_metrics WHERE garden_id = ?1")
            .bind(garden.to_string())
            .fetch_one(&self.db)
            .await?;
        Ok(row.try_get("n")?)
    }

    /// Drop metrics older than the retention window.
    pub async fn prune_slot_metrics(&self, before: Timestamp) -> Result<u64> {
        let result = sqlx::query("DELETE FROM slot_metrics WHERE at < ?1")
            .bind(ts::encode(before))
            .execute(&self.db)
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gardyn_auth::EmailAddress;
    use gardyn_core::{DeviceModel, time::add_days};

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    async fn fixture() -> (Store, GardenId) {
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
        (store, garden.id)
    }

    async fn frame(store: &Store, garden: GardenId, at: Timestamp) -> Uuid {
        // A 1×1 PNG is enough; this table only needs a frame row to reference.
        const PNG: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
            0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        store
            .put_frame(crate::frames::NewFrame {
                garden,
                captured_at: at,
                width: 1,
                height: 1,
                light_duty_milli: Some(800),
                comparable: true,
                source: crate::frames::FrameSource::Agent,
                bytes: PNG,
            })
            .await
            .unwrap()
            .unwrap()
            .id
    }

    fn metrics(slot: u8, at: Timestamp, area: f32) -> SlotMetrics {
        let mut m = SlotMetrics::new(SlotId(slot), at, area);
        m.green_fraction = 0.6;
        m.plant_count = Some(2);
        m
    }

    #[tokio::test]
    async fn a_garden_has_no_roi_map_until_it_is_calibrated() {
        let (store, garden) = fixture().await;
        assert_eq!(store.roi_map(garden).await.unwrap(), None);

        store.save_roi_map(garden, r#"{"slots":[]}"#, t0()).await.unwrap();
        assert_eq!(
            store.roi_map(garden).await.unwrap().as_deref(),
            Some(r#"{"slots":[]}"#)
        );
    }

    #[tokio::test]
    async fn recalibrating_replaces_the_map_rather_than_adding_one() {
        let (store, garden) = fixture().await;
        store.save_roi_map(garden, "first", t0()).await.unwrap();
        store.save_roi_map(garden, "second", add_days(t0(), 1.0)).await.unwrap();
        assert_eq!(store.roi_map(garden).await.unwrap().as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn clearing_the_map_keeps_the_history_it_produced() {
        // Recalibrating should not erase last month's growth curve. The measurements
        // were true when they were taken.
        let (store, garden) = fixture().await;
        store.save_roi_map(garden, "map", t0()).await.unwrap();
        let f = frame(&store, garden, t0()).await;
        store
            .record_slot_metrics(garden, f, &[metrics(0, t0(), 120.0)])
            .await
            .unwrap();

        store.clear_roi_map(garden).await.unwrap();
        assert_eq!(store.roi_map(garden).await.unwrap(), None);
        assert_eq!(store.slot_metrics_count(garden).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn re_analysing_a_frame_replaces_its_own_rows() {
        let (store, garden) = fixture().await;
        let f = frame(&store, garden, t0()).await;
        store
            .record_slot_metrics(garden, f, &[metrics(0, t0(), 100.0)])
            .await
            .unwrap();
        store
            .record_slot_metrics(garden, f, &[metrics(0, t0(), 250.0)])
            .await
            .unwrap();

        assert_eq!(store.slot_metrics_count(garden).await.unwrap(), 1);
        let latest = store.latest_slot_metrics(garden).await.unwrap();
        assert_eq!(latest[0].canopy_area_cm2, 250.0);
    }

    #[tokio::test]
    async fn deleting_a_frame_deletes_what_was_measured_from_it() {
        // A measurement whose evidence is gone cannot be checked, so it should not
        // outlive the photograph.
        let (store, garden) = fixture().await;
        let f = frame(&store, garden, t0()).await;
        store
            .record_slot_metrics(garden, f, &[metrics(0, t0(), 100.0)])
            .await
            .unwrap();

        store.prune_frames(garden, 0.5, add_days(t0(), 1.0)).await.unwrap();
        assert_eq!(store.slot_metrics_count(garden).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn each_slot_reports_its_own_latest_even_from_different_frames() {
        // Slot 3 was in shadow this morning but measured fine yesterday. Requiring one
        // frame to supply every slot would throw that away.
        let (store, garden) = fixture().await;
        let yesterday = frame(&store, garden, t0()).await;
        let today = frame(&store, garden, add_days(t0(), 1.0)).await;

        store
            .record_slot_metrics(
                garden,
                yesterday,
                &[metrics(0, t0(), 50.0), metrics(3, t0(), 80.0)],
            )
            .await
            .unwrap();
        store
            .record_slot_metrics(garden, today, &[metrics(0, add_days(t0(), 1.0), 90.0)])
            .await
            .unwrap();

        let latest = store.latest_slot_metrics(garden).await.unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].canopy_area_cm2, 90.0, "slot 0 refreshed");
        assert_eq!(latest[1].canopy_area_cm2, 80.0, "slot 3 kept yesterday's");
    }

    #[tokio::test]
    async fn canopy_history_comes_back_oldest_first_and_windowed() {
        let (store, garden) = fixture().await;
        for day in 0..6 {
            let at = add_days(t0(), f64::from(day));
            let f = frame(&store, garden, at).await;
            store
                .record_slot_metrics(garden, f, &[metrics(1, at, 100.0 + day as f32 * 10.0)])
                .await
                .unwrap();
        }

        let all = store
            .canopy_history(garden, SlotId(1), t0())
            .await
            .unwrap();
        assert_eq!(all.len(), 6);
        assert!(all.windows(2).all(|w| w[0].0 <= w[1].0));
        assert_eq!(all[0].1, 100.0);

        let recent = store
            .canopy_history(garden, SlotId(1), add_days(t0(), 3.0))
            .await
            .unwrap();
        assert_eq!(recent.len(), 3);
    }

    #[tokio::test]
    async fn algae_keeps_only_the_most_recent_reading_per_frame() {
        let (store, garden) = fixture().await;
        let f = frame(&store, garden, t0()).await;
        store
            .record_algae(garden, f, AlgaeReading { at: t0(), coverage: 0.05 })
            .await
            .unwrap();
        store
            .record_algae(garden, f, AlgaeReading { at: t0(), coverage: 0.31 })
            .await
            .unwrap();

        let latest = store.latest_algae(garden).await.unwrap().unwrap();
        assert_eq!(latest.coverage, 0.31);
        assert!(latest.is_urgent());
    }

    #[tokio::test]
    async fn one_gardens_metrics_are_invisible_to_another() {
        let (store, mine) = fixture().await;
        let other = store
            .create_user(
                EmailAddress::parse("someone@example.com").unwrap(),
                "Someone",
                "a long enough password",
                t0(),
            )
            .await
            .unwrap();
        let theirs = store
            .create_garden("Theirs", DeviceModel::Studio2, "UTC", other.id, t0())
            .await
            .unwrap();

        let f = frame(&store, mine, t0()).await;
        store
            .record_slot_metrics(mine, f, &[metrics(0, t0(), 100.0)])
            .await
            .unwrap();

        assert_eq!(store.latest_slot_metrics(theirs.id).await.unwrap().len(), 0);
        assert_eq!(store.latest_algae(theirs.id).await.unwrap(), None);
        assert_eq!(store.roi_map(theirs.id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn pruning_drops_old_metrics_and_keeps_recent_ones() {
        let (store, garden) = fixture().await;
        for day in [0.0, 200.0, 500.0] {
            let at = add_days(t0(), day);
            let f = frame(&store, garden, at).await;
            store
                .record_slot_metrics(garden, f, &[metrics(0, at, 100.0)])
                .await
                .unwrap();
        }
        let removed = store
            .prune_slot_metrics(add_days(t0(), 300.0))
            .await
            .unwrap();
        assert_eq!(removed, 2);
        assert_eq!(store.slot_metrics_count(garden).await.unwrap(), 1);
    }
}
