//! Per-garden operator settings, and what they are costing on disk.
//!
//! Camera frames are the only thing in this system that grows without bound: one an
//! hour is roughly 8,700 files a year per garden, and the pruning helper that would
//! deal with them existed for a long time without anything calling it. Retention is a
//! per-garden decision because gardens differ — one you are debugging deserves more
//! history than one that has been fine for months — and it belongs in the UI next to
//! the number it controls, which is how much disk you are using.

use crate::{Result, Store, ts};
use garden_core::GardenId;
use jiff::Timestamp;
use sqlx::Row;

/// How long frames are kept when a garden has not said otherwise.
///
/// Ninety days is about 2,200 frames — comfortably inside the deployment's 40 GB disk,
/// and long enough to watch a crop go from seed to harvest in pictures.
pub const DEFAULT_FRAME_RETENTION_DAYS: i64 = 90;

/// Bounds on what the UI will accept.
///
/// The floor is not zero: a setting that deletes every photograph the moment it is
/// saved is a foot-gun, not a configuration. Turn the camera off at the agent instead.
pub const MIN_FRAME_RETENTION_DAYS: i64 = 7;
pub const MAX_FRAME_RETENTION_DAYS: i64 = 3_650;

/// What a garden is using.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameStorage {
    pub count: i64,
    pub bytes: i64,
    pub oldest: Option<Timestamp>,
    pub newest: Option<Timestamp>,
}

impl FrameStorage {
    pub fn megabytes(&self) -> f64 {
        self.bytes as f64 / 1_048_576.0
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

impl Store {
    /// How long this garden keeps frames.
    pub async fn frame_retention_days(&self, garden: GardenId) -> Result<i64> {
        let row = sqlx::query("SELECT frame_retention_days FROM garden_settings WHERE garden_id = ?1")
            .bind(garden.to_string())
            .fetch_optional(&self.db)
            .await?;
        Ok(match row {
            Some(row) => row.try_get::<i64, _>("frame_retention_days")?,
            None => DEFAULT_FRAME_RETENTION_DAYS,
        })
    }

    /// Set it, clamped to something sane.
    ///
    /// Clamped rather than rejected because this arrives from a number field in a form
    /// and the bounds are about protecting the operator from a typo, not about
    /// validating a protocol.
    pub async fn set_frame_retention_days(
        &self,
        garden: GardenId,
        days: i64,
        now: Timestamp,
    ) -> Result<i64> {
        let days = days.clamp(MIN_FRAME_RETENTION_DAYS, MAX_FRAME_RETENTION_DAYS);
        sqlx::query(
            "INSERT INTO garden_settings (garden_id, frame_retention_days, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(garden_id) DO UPDATE SET
                frame_retention_days = excluded.frame_retention_days,
                updated_at = excluded.updated_at",
        )
        .bind(garden.to_string())
        .bind(days)
        .bind(ts::encode(now))
        .execute(&self.db)
        .await?;
        Ok(days)
    }

    /// Frames held, and what they weigh.
    ///
    /// Summed from the `byte_size` column rather than by walking the directory: every
    /// file on disk has a row, the sum is indexed, and stat-ing eight thousand files to
    /// render a settings page would be absurd.
    pub async fn frame_storage(&self, garden: GardenId) -> Result<FrameStorage> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS n, COALESCE(SUM(byte_size), 0) AS bytes,
                    MIN(captured_at) AS oldest, MAX(captured_at) AS newest
             FROM frames WHERE garden_id = ?1",
        )
        .bind(garden.to_string())
        .fetch_one(&self.db)
        .await?;

        Ok(FrameStorage {
            count: row.try_get("n")?,
            bytes: row.try_get("bytes")?,
            oldest: ts::decode_opt(row.try_get("oldest")?)?,
            newest: ts::decode_opt(row.try_get("newest")?)?,
        })
    }

    /// What tightening retention to `keep_days` would delete.
    ///
    /// The whole point of a separate query: the confirmation has to state the real
    /// number before anything is removed, not afterwards.
    pub async fn frames_older_than(
        &self,
        garden: GardenId,
        keep_days: f64,
        now: Timestamp,
    ) -> Result<FrameStorage> {
        let cutoff = garden_core::time::add_days(now, -keep_days);
        let row = sqlx::query(
            "SELECT COUNT(*) AS n, COALESCE(SUM(byte_size), 0) AS bytes,
                    MIN(captured_at) AS oldest, MAX(captured_at) AS newest
             FROM frames WHERE garden_id = ?1 AND captured_at < ?2",
        )
        .bind(garden.to_string())
        .bind(ts::encode(cutoff))
        .fetch_one(&self.db)
        .await?;

        Ok(FrameStorage {
            count: row.try_get("n")?,
            bytes: row.try_get("bytes")?,
            oldest: ts::decode_opt(row.try_get("oldest")?)?,
            newest: ts::decode_opt(row.try_get("newest")?)?,
        })
    }

    /// Size of the database file, from SQLite rather than the filesystem.
    ///
    /// `Store` only knows a connection URL, which may not be a path at all — and this
    /// figure is the one that matters for a backup anyway.
    pub async fn database_bytes(&self) -> Result<i64> {
        let row = sqlx::query(
            "SELECT (SELECT * FROM pragma_page_count()) * (SELECT * FROM pragma_page_size())
                    AS bytes",
        )
        .fetch_one(&self.db)
        .await?;
        Ok(row.try_get("bytes")?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use garden_auth::EmailAddress;
    use garden_core::{DeviceModel, time::add_days};
    use crate::test_support::*;

    #[tokio::test]
    async fn a_garden_starts_at_the_default_retention() {
        let (store, garden) = fixture().await;
        assert_eq!(
            store.frame_retention_days(garden).await.unwrap(),
            DEFAULT_FRAME_RETENTION_DAYS
        );
    }

    #[tokio::test]
    async fn retention_is_per_garden() {
        let (store, mine) = fixture().await;
        let other = store
            .create_user(
                EmailAddress::parse("other@example.com").unwrap(),
                "Other",
                "a long enough password",
                t0(),
            )
            .await
            .unwrap();
        let theirs = store
            .create_garden("Theirs", DeviceModel::Studio2, "UTC", other.id, t0())
            .await
            .unwrap();

        store.set_frame_retention_days(mine, 30, t0()).await.unwrap();
        assert_eq!(store.frame_retention_days(mine).await.unwrap(), 30);
        assert_eq!(
            store.frame_retention_days(theirs.id).await.unwrap(),
            DEFAULT_FRAME_RETENTION_DAYS
        );
    }

    #[tokio::test]
    async fn a_typo_is_clamped_rather_than_stored() {
        // Zero would delete every photograph the moment it was saved.
        let (store, garden) = fixture().await;
        assert_eq!(
            store.set_frame_retention_days(garden, 0, t0()).await.unwrap(),
            MIN_FRAME_RETENTION_DAYS
        );
        assert_eq!(
            store
                .set_frame_retention_days(garden, 99_999, t0())
                .await
                .unwrap(),
            MAX_FRAME_RETENTION_DAYS
        );
    }

    #[tokio::test]
    async fn storage_reports_what_is_held() {
        let (store, garden) = fixture().await;
        assert!(store.frame_storage(garden).await.unwrap().is_empty());

        for day in 0..4 {
            frame_at(&store, garden, add_days(t0(), f64::from(day))).await;
        }
        let storage = store.frame_storage(garden).await.unwrap();
        assert_eq!(storage.count, 4);
        assert!(storage.bytes > 0);
        assert_eq!(storage.oldest, Some(t0()));
        assert_eq!(storage.newest, Some(add_days(t0(), 3.0)));
    }

    #[tokio::test]
    async fn a_tightened_window_reports_what_it_would_delete_before_deleting_it() {
        // The confirmation has to state the real number, so this is asked first and
        // the prune happens second.
        let (store, garden) = fixture().await;
        for day in 0..10 {
            frame_at(&store, garden, add_days(t0(), f64::from(day))).await;
        }
        let now = add_days(t0(), 10.0);

        let doomed = store.frames_older_than(garden, 5.0, now).await.unwrap();
        assert_eq!(doomed.count, 5, "days 0-4 are older than five days");
        assert!(doomed.bytes > 0);
        assert_eq!(doomed.oldest, Some(t0()));

        // Nothing has actually gone yet.
        assert_eq!(store.frame_storage(garden).await.unwrap().count, 10);

        let removed = store.prune_frames(garden, 5.0, now).await.unwrap();
        assert_eq!(removed, 5);
        assert_eq!(store.frame_storage(garden).await.unwrap().count, 5);
    }

    #[tokio::test]
    async fn a_loosened_window_would_delete_nothing() {
        let (store, garden) = fixture().await;
        for day in 0..5 {
            frame_at(&store, garden, add_days(t0(), f64::from(day))).await;
        }
        let doomed = store
            .frames_older_than(garden, 400.0, add_days(t0(), 5.0))
            .await
            .unwrap();
        assert_eq!(doomed.count, 0);
        assert_eq!(doomed.bytes, 0);
    }

    #[tokio::test]
    async fn one_gardens_storage_is_invisible_to_another() {
        let (store, mine) = fixture().await;
        let other = store
            .create_user(
                EmailAddress::parse("other@example.com").unwrap(),
                "Other",
                "a long enough password",
                t0(),
            )
            .await
            .unwrap();
        let theirs = store
            .create_garden("Theirs", DeviceModel::Studio2, "UTC", other.id, t0())
            .await
            .unwrap();

        frame_at(&store, mine, t0()).await;
        assert_eq!(store.frame_storage(mine).await.unwrap().count, 1);
        assert!(store.frame_storage(theirs.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_database_reports_its_own_size() {
        let (store, _) = fixture().await;
        let bytes = store.database_bytes().await.unwrap();
        assert!(bytes > 0, "a migrated database is not zero bytes");
        assert_eq!(bytes % 512, 0, "should be a whole number of pages");
    }
}
