//! SQLite persistence.
//!
//! One `Store` wraps the pool and exposes repositories. Queries are runtime-checked
//! rather than macro-checked so the crate builds without a live database — the
//! deployment is a single self-hosted binary, and requiring `DATABASE_URL` at compile
//! time would make cross-compiling for the Pi needlessly painful.

pub mod accounts;
pub mod fleet;
pub mod frames;
pub mod gardens;
pub mod notifications;
pub mod plantings;
pub mod readings;
pub mod schema;
pub mod tank;
pub mod tasks;
pub mod vision;

use frames::FrameStore;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Pool, Sqlite};
use std::path::PathBuf;
use std::str::FromStr;

pub type Db = Pool<Sqlite>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("stored value is not valid: {0}")]
    Corrupt(String),
    #[error("that email address is already registered")]
    EmailTaken,
    #[error("not found")]
    NotFound,
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Clone)]
pub struct Store {
    pub db: Db,
    /// Where camera frame bytes live. Kept out of SQLite so backups stay small.
    pub frames: FrameStore,
}

impl Store {
    /// Open (or create) a database at `path`, with frame bytes alongside it.
    pub async fn open(path: &str) -> Result<Self> {
        Self::open_with(path, default_frame_root()).await
    }

    /// Open with an explicit directory for frame bytes.
    pub async fn open_with(path: &str, frame_root: impl Into<PathBuf>) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(path)
            .map_err(StoreError::Database)?
            .create_if_missing(true)
            // WAL so readers are never blocked by the writer, and so the
            // `VACUUM INTO` backup below produces a coherent file.
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));

        let db = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;

        let store = Self {
            db,
            frames: FrameStore::new(frame_root),
        };
        store.migrate().await?;
        Ok(store)
    }

    /// An ephemeral database with a throwaway frame directory, for tests.
    pub async fn in_memory() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("garden-test-frames-{}", uuid::Uuid::new_v4()));
        Self::open_with("sqlite::memory:", root).await
    }

    async fn migrate(&self) -> Result<()> {
        let mut conn = self.db.acquire().await?;
        sqlx::raw_sql(schema::SCHEMA).execute(&mut *conn).await?;
        sqlx::raw_sql(schema::NORMALISE_TIMESTAMPS)
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    /// Consistent point-in-time copy, safe to snapshot.
    ///
    /// A Proxmox snapshot of a live WAL-mode SQLite file is not guaranteed coherent;
    /// this produces one that is.
    pub async fn backup_to(&self, path: &str) -> Result<()> {
        sqlx::query("VACUUM INTO ?1")
            .bind(path)
            .execute(&self.db)
            .await?;
        Ok(())
    }
}

fn default_frame_root() -> PathBuf {
    PathBuf::from("garden-data").join("frames")
}

/// Helpers for the RFC 3339 text used by every timestamp column.
pub(crate) mod ts {
    use crate::StoreError;
    use jiff::Timestamp;

    /// Fractional-second digits every stored timestamp carries.
    ///
    /// **Fixed width is the whole point.** Timestamps are compared as text by SQLite,
    /// and `jiff`'s `Display` prints the fewest digits it can — 0, 1, 3, 6 or 9. That
    /// makes lexical order disagree with chronological order: `"…:20Z"` sorts *after*
    /// `"…:20.5Z"`, because `'Z'` is greater than `'.'`. Every `BETWEEN`, every
    /// `at <= ?`, every `ORDER BY at` is wrong for the pairs that straddle a precision
    /// change, and it fails by silently omitting rows rather than by erroring.
    ///
    /// Nine digits, always, so text order is time order.
    pub const PRECISION: usize = 9;

    pub fn encode(t: Timestamp) -> String {
        format!("{t:.*}", PRECISION)
    }

    pub fn encode_opt(t: Option<Timestamp>) -> Option<String> {
        t.map(encode)
    }

    pub fn decode(s: &str) -> Result<Timestamp, StoreError> {
        s.parse()
            .map_err(|e| StoreError::Corrupt(format!("timestamp {s:?}: {e}")))
    }

    pub fn decode_opt(s: Option<String>) -> Result<Option<Timestamp>, StoreError> {
        s.as_deref().map(decode).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_in_memory_store_applies_its_schema() {
        let store = Store::in_memory().await.unwrap();
        let tables: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&store.db)
                .await
                .unwrap();
        let names: Vec<&str> = tables.iter().map(|(n,)| n.as_str()).collect();

        for expected in [
            "users",
            "sessions",
            "gardens",
            "memberships",
            "invitations",
            "tasks",
            "events",
            "action_grants",
            "components",
        ] {
            assert!(names.contains(&expected), "missing table {expected}");
        }
    }

    #[tokio::test]
    async fn foreign_keys_are_enforced() {
        // Without this pragma, deleting a user would silently orphan their
        // memberships and a stale grant could outlive the account.
        let store = Store::in_memory().await.unwrap();
        let (enabled,): (i64,) = sqlx::query_as("PRAGMA foreign_keys")
            .fetch_one(&store.db)
            .await
            .unwrap();
        assert_eq!(enabled, 1);
    }

    #[tokio::test]
    async fn timestamps_round_trip_as_text() {
        let now = jiff::Timestamp::from_second(1_700_000_000).unwrap();
        assert_eq!(ts::decode(&ts::encode(now)).unwrap(), now);
    }

    #[tokio::test]
    async fn a_corrupt_timestamp_is_an_error_not_a_panic() {
        assert!(ts::decode("not a timestamp").is_err());
    }

    #[test]
    fn encoded_timestamps_sort_the_same_way_as_the_instants_they_encode() {
        // The property every range query in this crate depends on, and the one that
        // `Timestamp::to_string()` does not have: it prints the fewest fractional
        // digits it can, so a whole second sorts *after* the same second plus half.
        use jiff::{SignedDuration, Timestamp};

        let base = Timestamp::from_second(1_700_000_000).unwrap();
        let offsets = [
            0i64,
            1,
            1_000,
            1_000_000,
            100_000_000,
            500_000_000,
            812_345_678,
            999_999_999,
        ];

        for pair in offsets.windows(2) {
            let (a, b) = (
                base + SignedDuration::from_nanos(pair[0]),
                base + SignedDuration::from_nanos(pair[1]),
            );
            assert!(a < b, "test data out of order");
            assert!(
                ts::encode(a) < ts::encode(b),
                "{} should sort before {}",
                ts::encode(a),
                ts::encode(b),
            );
        }
    }

    #[test]
    fn every_encoded_timestamp_is_the_same_width() {
        use jiff::{SignedDuration, Timestamp};
        let base = Timestamp::from_second(1_700_000_000).unwrap();
        for nanos in [0i64, 1, 1_000, 500_000_000, 999_999_999] {
            let encoded = ts::encode(base + SignedDuration::from_nanos(nanos));
            assert_eq!(encoded.len(), 30, "{encoded}");
            assert!(encoded.ends_with('Z'));
        }
    }

    #[tokio::test]
    async fn the_migration_repairs_timestamps_written_at_the_old_precision() {
        // Databases created before the precision was pinned hold a mix of widths.
        // Opening them again has to fix that, or half their history stays invisible
        // to every window query.
        let store = Store::in_memory().await.unwrap();
        for (id, written) in [
            ("a", "2024-03-01T10:00:00Z"),
            ("b", "2024-03-01T10:00:00.5Z"),
            ("c", "2024-03-01T10:00:00.812345Z"),
            ("d", "2024-03-01T10:00:00.000000000Z"),
        ] {
            sqlx::query("INSERT INTO gardens (id, name, model, timezone, created_at) VALUES (?1, ?1, 'studio_2', 'UTC', ?2)")
                .bind(id)
                .bind(written)
                .execute(&store.db)
                .await
                .unwrap();
        }

        store.migrate().await.unwrap();

        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT id, created_at FROM gardens ORDER BY created_at, id")
                .fetch_all(&store.db)
                .await
                .unwrap();

        for (id, at) in &rows {
            assert_eq!(at.len(), 30, "{id} is still {at}");
        }
        // ...and now they sort by time rather than by punctuation.
        let order: Vec<&str> = rows.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(order, vec!["a", "d", "b", "c"]);
    }
}
