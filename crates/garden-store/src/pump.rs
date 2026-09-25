//! The pump's clean-system reference draw.
//!
//! Restriction is measured as a ratio against this, so it is the number the whole
//! diagnostic rests on — and it was a hardcoded placeholder until this module existed.
//!
//! Two states matter and they are not the same:
//!
//! - **No baseline at all.** Nothing has been measured, so no percentage can be
//!   reported. Silence, not a confident zero.
//! - **Pending.** A deep clean has voided the old reference and a new one is being
//!   learned from readings taken *after* the clean. Also silence, and for a better
//!   reason: the draw in the days before the clean is exactly what must not become
//!   the definition of clean.

use crate::{Result, Store, ts};
use garden_core::GardenId;
use jiff::Timestamp;
use sqlx::Row;

/// Where a baseline came from, because the two are not equally trustworthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineSource {
    /// Measured after a deep clean. A real clean-system reference.
    AfterDeepClean,
    /// Seeded from the first readings of a garden nobody had measured before.
    ///
    /// Usable, and honest about being weaker: if the system was already fouled when
    /// the agent arrived, this bakes that in and fouling will never look like
    /// fouling. The UI says which kind it is so a first deep clean can fix it.
    FirstReadings,
}

impl BaselineSource {
    pub fn slug(self) -> &'static str {
        match self {
            BaselineSource::AfterDeepClean => "after-deep-clean",
            BaselineSource::FirstReadings => "first-readings",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "after-deep-clean" => Some(BaselineSource::AfterDeepClean),
            "first-readings" => Some(BaselineSource::FirstReadings),
            _ => None,
        }
    }

    /// Short phrase for the dashboard, completing "measured …".
    pub fn label(self) -> &'static str {
        match self {
            BaselineSource::AfterDeepClean => "after a deep clean",
            BaselineSource::FirstReadings => "from the first readings",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredBaseline {
    /// `None` while a new one is being learned.
    pub nominal_ma: Option<f32>,
    pub source: BaselineSource,
    /// Set when a deep clean voided the old reference. Readings before this moment
    /// describe a system that was still dirty and must not be averaged in.
    pub pending_since: Option<Timestamp>,
    pub set_at: Timestamp,
}

impl Store {
    pub async fn pump_baseline(&self, garden: GardenId) -> Result<Option<StoredBaseline>> {
        let row = sqlx::query("SELECT * FROM pump_baseline WHERE garden_id = ?1")
            .bind(garden.to_string())
            .fetch_optional(&self.db)
            .await?;
        let Some(row) = row else { return Ok(None) };

        Ok(Some(StoredBaseline {
            nominal_ma: row.try_get::<Option<f64>, _>("nominal_ma")?.map(|v| v as f32),
            source: row
                .try_get::<String, _>("source")
                .ok()
                .and_then(|s| BaselineSource::parse(&s))
                .unwrap_or(BaselineSource::FirstReadings),
            pending_since: ts::decode_opt(row.try_get("pending_since")?)?,
            set_at: ts::decode(&row.try_get::<String, _>("set_at")?)?,
        }))
    }

    /// Record a measured clean-system draw.
    pub async fn set_pump_baseline(
        &self,
        garden: GardenId,
        nominal_ma: f32,
        source: BaselineSource,
        now: Timestamp,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO pump_baseline (garden_id, nominal_ma, source, pending_since, set_at)
             VALUES (?1, ?2, ?3, NULL, ?4)
             ON CONFLICT(garden_id) DO UPDATE SET nominal_ma = ?2, source = ?3,
                                                  pending_since = NULL, set_at = ?4",
        )
        .bind(garden.to_string())
        .bind(f64::from(nominal_ma))
        .bind(source.slug())
        .bind(ts::encode(now))
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Void the current reference after a deep clean, and start learning a new one.
    ///
    /// Deliberately does not measure anything. At the moment the task is ticked, every
    /// reading on record was taken while the system was still fouled; using them would
    /// define "clean" as the state that prompted the clean. The next reference comes
    /// from readings after `now`, which is why this stores a timestamp rather than a
    /// value.
    pub async fn invalidate_pump_baseline(&self, garden: GardenId, now: Timestamp) -> Result<()> {
        sqlx::query(
            "INSERT INTO pump_baseline (garden_id, nominal_ma, source, pending_since, set_at)
             VALUES (?1, NULL, ?2, ?3, ?3)
             ON CONFLICT(garden_id) DO UPDATE SET nominal_ma = NULL, source = ?2,
                                                  pending_since = ?3, set_at = ?3",
        )
        .bind(garden.to_string())
        .bind(BaselineSource::AfterDeepClean.slug())
        .bind(ts::encode(now))
        .execute(&self.db)
        .await?;
        Ok(())
    }
}
