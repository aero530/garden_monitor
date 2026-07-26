//! Gardens and their event log.

use crate::{Result, Store, StoreError, ts};
use gardyn_auth::{Membership, Role, UserId};
use gardyn_core::{DeviceModel, Garden, GardenId};
use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

fn model_from_str(s: &str) -> Result<DeviceModel> {
    Ok(match s {
        "studio2" => DeviceModel::Studio2,
        "studio1" => DeviceModel::Studio1,
        "home4" => DeviceModel::Home4,
        "home3" => DeviceModel::Home3,
        "simulated" => DeviceModel::Simulated,
        other => return Err(StoreError::Corrupt(format!("unknown model {other:?}"))),
    })
}

pub fn model_slug(model: DeviceModel) -> &'static str {
    match model {
        DeviceModel::Studio2 => "studio2",
        DeviceModel::Studio1 => "studio1",
        DeviceModel::Home4 => "home4",
        DeviceModel::Home3 => "home3",
        DeviceModel::Simulated => "simulated",
    }
}

fn garden_from_row(row: &SqliteRow) -> Result<Garden> {
    let id: String = row.try_get("id")?;
    let model: String = row.try_get("model")?;
    Ok(Garden {
        id: GardenId(
            Uuid::parse_str(&id).map_err(|e| StoreError::Corrupt(format!("garden id: {e}")))?,
        ),
        name: row.try_get("name")?,
        model: model_from_str(&model)?,
        timezone: row.try_get("timezone")?,
        created_at: ts::decode(&row.try_get::<String, _>("created_at")?)?,
    })
}

/// A garden as it appears in someone's list, with their role in it.
#[derive(Debug, Clone)]
pub struct GardenListing {
    pub garden: Garden,
    pub role: Role,
    /// How many people can see it. More than one means it is shared.
    pub member_count: i64,
}

impl GardenListing {
    pub fn is_shared(&self) -> bool {
        self.member_count > 1
    }

    /// Shared *with* the viewer, as opposed to shared *by* them.
    pub fn is_someone_elses(&self) -> bool {
        self.role != Role::Owner
    }
}

#[derive(Debug, Clone)]
pub struct EventRecord {
    pub kind: String,
    pub detail: Option<String>,
    pub actor: Option<UserId>,
    pub actor_name: Option<String>,
    pub occurred_at: Timestamp,
}

impl Store {
    /// Create a garden and make its creator the owner, atomically.
    ///
    /// A transaction because a garden with no owner is unreachable by anyone: there
    /// would be no membership row, so no actor could ever see it again.
    pub async fn create_garden(
        &self,
        name: &str,
        model: DeviceModel,
        timezone: &str,
        owner: UserId,
        now: Timestamp,
    ) -> Result<Garden> {
        let garden = Garden {
            id: GardenId::new(),
            name: name.trim().to_string(),
            model,
            timezone: timezone.to_string(),
            created_at: now,
        };

        let mut tx = self.db.begin().await?;

        sqlx::query(
            "INSERT INTO gardens (id, name, model, timezone, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(garden.id.to_string())
        .bind(&garden.name)
        .bind(model_slug(model))
        .bind(&garden.timezone)
        .bind(ts::encode(now))
        .execute(&mut *tx)
        .await?;

        let membership = Membership::founding_owner(garden.id, owner, now);
        sqlx::query(
            "INSERT INTO memberships (garden_id, user_id, role, granted_by, granted_at)
             VALUES (?1, ?2, 'owner', NULL, ?3)",
        )
        .bind(membership.garden.to_string())
        .bind(membership.user.to_string())
        .bind(ts::encode(now))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(garden)
    }

    /// Every garden on the server, for the notification sweep.
    ///
    /// Deliberately not membership-scoped: the dispatcher acts on behalf of the
    /// system, then filters per member. Nothing user-facing calls this.
    pub async fn all_gardens(&self) -> Result<Vec<Garden>> {
        let rows = sqlx::query("SELECT * FROM gardens ORDER BY created_at")
            .fetch_all(&self.db)
            .await?;
        rows.iter().map(garden_from_row).collect()
    }

    pub async fn find_garden(&self, id: GardenId) -> Result<Option<Garden>> {
        let row = sqlx::query("SELECT * FROM gardens WHERE id = ?1")
            .bind(id.to_string())
            .fetch_optional(&self.db)
            .await?;
        row.as_ref().map(garden_from_row).transpose()
    }

    /// Every garden a user can reach, owned first, then alphabetically.
    ///
    /// Driven by the membership join rather than by a caller-supplied list of ids, so
    /// there is no path by which a garden without a membership row appears here.
    pub async fn gardens_for_user(&self, user: UserId) -> Result<Vec<GardenListing>> {
        let rows = sqlx::query(
            "SELECT g.*, m.role AS m_role,
                    (SELECT COUNT(*) FROM memberships x WHERE x.garden_id = g.id) AS member_count
             FROM memberships m
             JOIN gardens g ON g.id = m.garden_id
             WHERE m.user_id = ?1",
        )
        .bind(user.to_string())
        .fetch_all(&self.db)
        .await?;

        let mut listings: Vec<GardenListing> = rows
            .iter()
            .map(|row| {
                let raw: String = row.try_get("m_role")?;
                Ok(GardenListing {
                    garden: garden_from_row(row)?,
                    role: raw
                        .parse()
                        .map_err(|_| StoreError::Corrupt(format!("role {raw:?}")))?,
                    member_count: row.try_get("member_count")?,
                })
            })
            .collect::<Result<_>>()?;

        listings.sort_by(|a, b| {
            b.role
                .cmp(&a.role)
                .then_with(|| a.garden.name.to_lowercase().cmp(&b.garden.name.to_lowercase()))
        });
        Ok(listings)
    }

    pub async fn rename_garden(&self, id: GardenId, name: &str, timezone: &str) -> Result<()> {
        sqlx::query("UPDATE gardens SET name = ?1, timezone = ?2 WHERE id = ?3")
            .bind(name.trim())
            .bind(timezone)
            .bind(id.to_string())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Delete a garden and everything hanging off it.
    ///
    /// Foreign keys cascade the rows, but camera frames also have bytes on disk, and
    /// those the database knows nothing about. Deleting the row and leaving the
    /// photographs behind would be a slow disk leak and, worse, would keep images of
    /// someone's home after they asked for them to be gone.
    pub async fn delete_garden(&self, id: GardenId) -> Result<()> {
        sqlx::query("DELETE FROM gardens WHERE id = ?1")
            .bind(id.to_string())
            .execute(&self.db)
            .await?;
        self.frames.remove_garden_directory(id);
        Ok(())
    }

    /// Move ownership to an existing member, demoting the current owner to steward.
    pub async fn transfer_ownership(
        &self,
        garden: GardenId,
        from: UserId,
        to: UserId,
        now: Timestamp,
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;

        // The recipient must already be a member — you cannot hand a garden to a
        // stranger by id alone.
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT role FROM memberships WHERE garden_id = ?1 AND user_id = ?2")
                .bind(garden.to_string())
                .bind(to.to_string())
                .fetch_optional(&mut *tx)
                .await?;
        if existing.is_none() {
            return Err(StoreError::NotFound);
        }

        sqlx::query(
            "UPDATE memberships SET role = 'owner', granted_by = ?1, granted_at = ?2
             WHERE garden_id = ?3 AND user_id = ?4",
        )
        .bind(from.to_string())
        .bind(ts::encode(now))
        .bind(garden.to_string())
        .bind(to.to_string())
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE memberships SET role = 'steward' WHERE garden_id = ?1 AND user_id = ?2",
        )
        .bind(garden.to_string())
        .bind(from.to_string())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    // --- Event log ----------------------------------------------------------------

    pub async fn log_event(
        &self,
        garden: GardenId,
        kind: &str,
        detail: Option<&str>,
        actor: Option<UserId>,
        at: Timestamp,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO events (id, garden_id, kind, detail, actor_id, occurred_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(garden.to_string())
        .bind(kind)
        .bind(detail)
        .bind(actor.map(|a| a.to_string()))
        .bind(ts::encode(at))
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn recent_events(&self, garden: GardenId, limit: i64) -> Result<Vec<EventRecord>> {
        let rows = sqlx::query(
            "SELECT e.kind, e.detail, e.actor_id, e.occurred_at, u.display_name, u.email
             FROM events e
             LEFT JOIN users u ON u.id = e.actor_id
             WHERE e.garden_id = ?1
             ORDER BY e.occurred_at DESC
             LIMIT ?2",
        )
        .bind(garden.to_string())
        .bind(limit)
        .fetch_all(&self.db)
        .await?;

        rows.iter()
            .map(|row| {
                let actor_id: Option<String> = row.try_get("actor_id")?;
                let display: Option<String> = row.try_get("display_name")?;
                let email: Option<String> = row.try_get("email")?;
                Ok(EventRecord {
                    kind: row.try_get("kind")?,
                    detail: row.try_get("detail")?,
                    actor: actor_id
                        .map(|a| {
                            Uuid::parse_str(&a)
                                .map(UserId)
                                .map_err(|e| StoreError::Corrupt(format!("actor id: {e}")))
                        })
                        .transpose()?,
                    actor_name: display
                        .filter(|d| !d.trim().is_empty())
                        .or(email),
                    occurred_at: ts::decode(&row.try_get::<String, _>("occurred_at")?)?,
                })
            })
            .collect()
    }
}
