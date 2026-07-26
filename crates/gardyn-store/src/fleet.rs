//! Servers and applications.
//!
//! The edge agent on each Pi, the MQTT broker, the self-hosted ntfy server, the local
//! VLM, and the brain itself. Everything reports a heartbeat; health is derived from
//! silence rather than from anything claiming to be healthy, because a component that
//! has crashed cannot tell you it has crashed.

use crate::{Result, Store, StoreError, ts};
use gardyn_core::GardenId;
use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Health {
    /// Heartbeat is current.
    Up,
    /// Reporting, but reporting a problem itself.
    Degraded,
    /// Overdue. Presumed down.
    Down,
    /// Registered but never heard from.
    Unknown,
}

impl Health {
    pub fn label(self) -> &'static str {
        match self {
            Health::Up => "up",
            Health::Degraded => "degraded",
            Health::Down => "down",
            Health::Unknown => "unknown",
        }
    }

    pub fn is_problem(self) -> bool {
        matches!(self, Health::Degraded | Health::Down)
    }
}

#[derive(Debug, Clone)]
pub struct Component {
    pub id: Uuid,
    /// `None` for shared infrastructure that is not tied to one device.
    pub garden: Option<GardenId>,
    pub name: String,
    pub kind: String,
    pub version: Option<String>,
    pub endpoint: Option<String>,
    /// What the component last said about itself.
    pub reported_status: String,
    pub detail: Option<String>,
    pub heartbeat_seconds: i64,
    pub last_seen_at: Option<Timestamp>,
}

impl Component {
    /// Health as of `now`.
    ///
    /// A component is down when it has missed its heartbeat window with a grace
    /// factor, not the instant a beat is late — one dropped packet on a Pi Zero's
    /// wifi should not page anyone.
    pub fn health(&self, now: Timestamp) -> Health {
        let Some(last) = self.last_seen_at else {
            return Health::Unknown;
        };
        let elapsed = now.as_second() - last.as_second();
        let allowed = (self.heartbeat_seconds as f64 * Self::GRACE_FACTOR) as i64;

        if elapsed > allowed {
            Health::Down
        } else if self.reported_status != "ok" {
            Health::Degraded
        } else {
            Health::Up
        }
    }

    /// Missing this many heartbeats before a component is presumed down.
    const GRACE_FACTOR: f64 = 2.5;

    pub fn seconds_since_seen(&self, now: Timestamp) -> Option<i64> {
        self.last_seen_at
            .map(|last| (now.as_second() - last.as_second()).max(0))
    }
}

fn component_from_row(row: &SqliteRow) -> Result<Component> {
    let id: String = row.try_get("id")?;
    let garden: Option<String> = row.try_get("garden_id")?;
    Ok(Component {
        id: Uuid::parse_str(&id).map_err(|e| StoreError::Corrupt(format!("component id: {e}")))?,
        garden: garden
            .map(|g| {
                Uuid::parse_str(&g)
                    .map(GardenId)
                    .map_err(|e| StoreError::Corrupt(format!("garden: {e}")))
            })
            .transpose()?,
        name: row.try_get("name")?,
        kind: row.try_get("kind")?,
        version: row.try_get("version")?,
        endpoint: row.try_get("endpoint")?,
        reported_status: row.try_get("status")?,
        detail: row.try_get("detail")?,
        heartbeat_seconds: row.try_get("heartbeat_seconds")?,
        last_seen_at: ts::decode_opt(row.try_get("last_seen_at")?)?,
    })
}

impl Store {
    /// Register a component, or refresh it if it already exists.
    ///
    /// Keyed on (garden, name) rather than on a generated id so that a restarted
    /// agent re-registers into its existing row instead of creating a duplicate every
    /// time the Pi reboots.
    pub async fn register_component(
        &self,
        garden: Option<GardenId>,
        name: &str,
        kind: &str,
        heartbeat_seconds: i64,
        endpoint: Option<&str>,
        now: Timestamp,
    ) -> Result<Uuid> {
        let existing: Option<(String,)> = sqlx::query_as(
            "SELECT id FROM components
             WHERE name = ?1 AND (garden_id IS ?2 OR garden_id = ?2)",
        )
        .bind(name)
        .bind(garden.map(|g| g.to_string()))
        .fetch_optional(&self.db)
        .await?;

        if let Some((id,)) = existing {
            sqlx::query(
                "UPDATE components SET kind = ?1, heartbeat_seconds = ?2, endpoint = ?3
                 WHERE id = ?4",
            )
            .bind(kind)
            .bind(heartbeat_seconds)
            .bind(endpoint)
            .bind(&id)
            .execute(&self.db)
            .await?;
            return Uuid::parse_str(&id)
                .map_err(|e| StoreError::Corrupt(format!("component id: {e}")));
        }

        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO components
                (id, garden_id, name, kind, endpoint, heartbeat_seconds, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(id.to_string())
        .bind(garden.map(|g| g.to_string()))
        .bind(name)
        .bind(kind)
        .bind(endpoint)
        .bind(heartbeat_seconds)
        .bind(ts::encode(now))
        .execute(&self.db)
        .await?;
        Ok(id)
    }

    /// Record a heartbeat.
    pub async fn heartbeat(
        &self,
        id: Uuid,
        status: &str,
        version: Option<&str>,
        detail: Option<&str>,
        now: Timestamp,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE components SET status = ?1, version = COALESCE(?2, version),
                detail = ?3, last_seen_at = ?4
             WHERE id = ?5",
        )
        .bind(status)
        .bind(version)
        .bind(detail)
        .bind(ts::encode(now))
        .bind(id.to_string())
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Every registered component, problems first.
    pub async fn components(&self, now: Timestamp) -> Result<Vec<Component>> {
        let rows = sqlx::query("SELECT * FROM components")
            .fetch_all(&self.db)
            .await?;
        let mut components: Vec<Component> =
            rows.iter().map(component_from_row).collect::<Result<_>>()?;

        components.sort_by(|a, b| {
            b.health(now)
                .is_problem()
                .cmp(&a.health(now).is_problem())
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(components)
    }

    pub async fn components_for(&self, garden: GardenId) -> Result<Vec<Component>> {
        let rows = sqlx::query("SELECT * FROM components WHERE garden_id = ?1 ORDER BY name")
            .bind(garden.to_string())
            .fetch_all(&self.db)
            .await?;
        rows.iter().map(component_from_row).collect()
    }

    pub async fn delete_component(&self, id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM components WHERE id = ?1")
            .bind(id.to_string())
            .execute(&self.db)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn component(last_seen: Option<Timestamp>, status: &str) -> Component {
        Component {
            id: Uuid::new_v4(),
            garden: None,
            name: "mosquitto".into(),
            kind: "broker".into(),
            version: None,
            endpoint: None,
            reported_status: status.into(),
            detail: None,
            heartbeat_seconds: 60,
            last_seen_at: last_seen,
        }
    }

    #[test]
    fn a_component_that_never_reported_is_unknown_not_down() {
        assert_eq!(component(None, "ok").health(t0()), Health::Unknown);
    }

    #[test]
    fn a_recent_heartbeat_is_up() {
        let c = component(Some(t0()), "ok");
        assert_eq!(c.health(t0()), Health::Up);
    }

    #[test]
    fn one_missed_beat_is_tolerated() {
        // A dropped packet on a Pi Zero's wifi must not page anyone.
        let c = component(Some(t0()), "ok");
        let slightly_late = Timestamp::from_second(t0().as_second() + 90).unwrap();
        assert_eq!(c.health(slightly_late), Health::Up);
    }

    #[test]
    fn sustained_silence_is_down() {
        let c = component(Some(t0()), "ok");
        let much_later = Timestamp::from_second(t0().as_second() + 600).unwrap();
        assert_eq!(c.health(much_later), Health::Down);
        assert!(c.health(much_later).is_problem());
    }

    #[test]
    fn a_component_reporting_a_problem_is_degraded_not_up() {
        let c = component(Some(t0()), "sensor timeout");
        assert_eq!(c.health(t0()), Health::Degraded);
        assert!(c.health(t0()).is_problem());
    }

    #[test]
    fn silence_outranks_a_stale_ok() {
        // A component that said "ok" and then died must read as down, not up.
        let c = component(Some(t0()), "ok");
        let later = Timestamp::from_second(t0().as_second() + 10_000).unwrap();
        assert_eq!(c.health(later), Health::Down);
    }

    #[test]
    fn time_since_seen_never_goes_negative() {
        let c = component(Some(t0()), "ok");
        let earlier = Timestamp::from_second(t0().as_second() - 500).unwrap();
        assert_eq!(c.seconds_since_seen(earlier), Some(0));
    }
}
