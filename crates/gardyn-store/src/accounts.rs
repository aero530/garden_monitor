//! Accounts, sessions, memberships, and invitations.

use crate::{Result, Store, StoreError, ts};
use gardyn_auth::{
    Actor, EmailAddress, Invitation, InvitationId, Membership, PasswordDigest, Role, SecretToken,
    Session, SessionId, User, UserId, hash_password, verify_password,
};
use gardyn_core::GardenId;
use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

fn uuid(row: &SqliteRow, column: &str) -> Result<Uuid> {
    let raw: String = row.try_get(column)?;
    Uuid::parse_str(&raw).map_err(|e| StoreError::Corrupt(format!("{column}: {e}")))
}

fn opt_uuid(row: &SqliteRow, column: &str) -> Result<Option<Uuid>> {
    let raw: Option<String> = row.try_get(column)?;
    raw.map(|r| Uuid::parse_str(&r).map_err(|e| StoreError::Corrupt(format!("{column}: {e}"))))
        .transpose()
}

fn role(row: &SqliteRow, column: &str) -> Result<Role> {
    let raw: String = row.try_get(column)?;
    raw.parse()
        .map_err(|_| StoreError::Corrupt(format!("{column}: unknown role {raw:?}")))
}

fn user_from_row(row: &SqliteRow) -> Result<User> {
    let email: String = row.try_get("email")?;
    Ok(User {
        id: UserId(uuid(row, "id")?),
        email: EmailAddress::parse(&email)
            .map_err(|e| StoreError::Corrupt(format!("email {email:?}: {e}")))?,
        display_name: row.try_get("display_name")?,
        is_admin: row.try_get::<i64, _>("is_admin")? != 0,
        created_at: ts::decode(&row.try_get::<String, _>("created_at")?)?,
        disabled_at: ts::decode_opt(row.try_get("disabled_at")?)?,
    })
}

/// A member of a garden, as shown on the sharing page.
#[derive(Debug, Clone)]
pub struct MemberView {
    pub user: User,
    pub role: Role,
    pub granted_at: Timestamp,
    pub granted_by: Option<UserId>,
}

impl Store {
    // --- Users ------------------------------------------------------------------

    pub async fn user_count(&self) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&self.db)
            .await?;
        Ok(n)
    }

    /// Register an account.
    ///
    /// The very first account becomes the server administrator, which is how a fresh
    /// self-hosted deployment bootstraps without a seeded password in a config file.
    pub async fn create_user(
        &self,
        email: EmailAddress,
        display_name: &str,
        password: &str,
        now: Timestamp,
    ) -> Result<User> {
        let digest = hash_password(password)
            .map_err(|e| StoreError::Corrupt(format!("password hashing: {e}")))?;
        let is_admin = self.user_count().await? == 0;

        let user = User {
            id: UserId::new(),
            email,
            display_name: display_name.trim().to_string(),
            is_admin,
            created_at: now,
            disabled_at: None,
        };

        let result = sqlx::query(
            "INSERT INTO users (id, email, display_name, password_digest, is_admin, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(user.id.to_string())
        .bind(user.email.as_str())
        .bind(&user.display_name)
        .bind(digest.as_str())
        .bind(i64::from(user.is_admin))
        .bind(ts::encode(now))
        .execute(&self.db)
        .await;

        match result {
            Ok(_) => Ok(user),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(StoreError::EmailTaken),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn find_user_by_email(&self, email: &EmailAddress) -> Result<Option<User>> {
        let row = sqlx::query("SELECT * FROM users WHERE email = ?1")
            .bind(email.as_str())
            .fetch_optional(&self.db)
            .await?;
        row.as_ref().map(user_from_row).transpose()
    }

    pub async fn find_user(&self, id: UserId) -> Result<Option<User>> {
        let row = sqlx::query("SELECT * FROM users WHERE id = ?1")
            .bind(id.to_string())
            .fetch_optional(&self.db)
            .await?;
        row.as_ref().map(user_from_row).transpose()
    }

    /// Verify a password and, on success, open a session.
    ///
    /// A missing account still pays the cost of an Argon2 verification against a
    /// throwaway hash. Without that, response time reveals which addresses are
    /// registered — a free account-enumeration oracle on a login form.
    pub async fn authenticate(
        &self,
        email: &EmailAddress,
        password: &str,
        now: Timestamp,
        user_agent: Option<String>,
    ) -> Result<Option<(User, SecretToken)>> {
        let row = sqlx::query("SELECT * FROM users WHERE email = ?1")
            .bind(email.as_str())
            .fetch_optional(&self.db)
            .await?;

        let Some(row) = row else {
            let _ = verify_password(password, &decoy_digest());
            return Ok(None);
        };

        let stored = PasswordDigest::from_stored(row.try_get::<String, _>("password_digest")?);
        if !verify_password(password, &stored) {
            return Ok(None);
        }

        let user = user_from_row(&row)?;
        if !user.is_active() {
            return Ok(None);
        }

        let token = self.open_session(user.id, now, user_agent).await?;
        Ok(Some((user, token)))
    }

    // --- Sessions ---------------------------------------------------------------

    pub async fn open_session(
        &self,
        user: UserId,
        now: Timestamp,
        user_agent: Option<String>,
    ) -> Result<SecretToken> {
        let (session, token) = Session::issue(user, now, user_agent);
        sqlx::query(
            "INSERT INTO sessions (id, user_id, digest, created_at, expires_at, last_seen_at, user_agent)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(session.id.0.to_string())
        .bind(session.user.to_string())
        .bind(session.digest.as_str())
        .bind(ts::encode(session.created_at))
        .bind(ts::encode(session.expires_at))
        .bind(ts::encode(session.last_seen_at))
        .bind(session.user_agent.as_deref())
        .execute(&self.db)
        .await?;
        Ok(token)
    }

    /// Resolve a cookie into a fully-populated [`Actor`].
    ///
    /// Returns `None` for anything not currently valid — unknown, expired, or
    /// belonging to a disabled account — so callers cannot accidentally treat a stale
    /// session as authenticated.
    pub async fn actor_for_token(&self, token: &SecretToken, now: Timestamp) -> Result<Option<Actor>> {
        let digest = token.digest();
        let row = sqlx::query("SELECT id, user_id, expires_at FROM sessions WHERE digest = ?1")
            .bind(digest.as_str())
            .fetch_optional(&self.db)
            .await?;

        let Some(row) = row else { return Ok(None) };
        let expires_at = ts::decode(&row.try_get::<String, _>("expires_at")?)?;
        if now >= expires_at {
            // Tidy up as we go rather than needing a separate reaper for the common case.
            let id: String = row.try_get("id")?;
            sqlx::query("DELETE FROM sessions WHERE id = ?1")
                .bind(id)
                .execute(&self.db)
                .await?;
            return Ok(None);
        }

        let user_id = UserId(uuid(&row, "user_id")?);
        let Some(user) = self.find_user(user_id).await? else {
            return Ok(None);
        };
        if !user.is_active() {
            return Ok(None);
        }

        sqlx::query("UPDATE sessions SET last_seen_at = ?1 WHERE digest = ?2")
            .bind(ts::encode(now))
            .bind(digest.as_str())
            .execute(&self.db)
            .await?;

        let memberships = self.memberships_of_user(user_id).await?;
        Ok(Some(Actor::new(user, memberships)))
    }

    pub async fn close_session(&self, token: &SecretToken) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE digest = ?1")
            .bind(token.digest().as_str())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Sign a user out everywhere. Used when a password changes or an account is
    /// disabled — a revoked account with live cookies is not revoked.
    pub async fn close_all_sessions(&self, user: UserId) -> Result<u64> {
        let result = sqlx::query("DELETE FROM sessions WHERE user_id = ?1")
            .bind(user.to_string())
            .execute(&self.db)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn purge_expired_sessions(&self, now: Timestamp) -> Result<u64> {
        let result = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?1")
            .bind(ts::encode(now))
            .execute(&self.db)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn sessions_of(&self, user: UserId) -> Result<Vec<Session>> {
        let rows = sqlx::query("SELECT * FROM sessions WHERE user_id = ?1 ORDER BY last_seen_at DESC")
            .bind(user.to_string())
            .fetch_all(&self.db)
            .await?;

        rows.iter()
            .map(|row| {
                Ok(Session {
                    id: SessionId(uuid(row, "id")?),
                    user,
                    digest: gardyn_auth::TokenDigest::from_stored(
                        row.try_get::<String, _>("digest")?,
                    ),
                    created_at: ts::decode(&row.try_get::<String, _>("created_at")?)?,
                    expires_at: ts::decode(&row.try_get::<String, _>("expires_at")?)?,
                    last_seen_at: ts::decode(&row.try_get::<String, _>("last_seen_at")?)?,
                    user_agent: row.try_get("user_agent")?,
                })
            })
            .collect()
    }

    // --- Memberships ------------------------------------------------------------

    pub async fn memberships_of_user(&self, user: UserId) -> Result<Vec<Membership>> {
        let rows = sqlx::query("SELECT * FROM memberships WHERE user_id = ?1")
            .bind(user.to_string())
            .fetch_all(&self.db)
            .await?;

        rows.iter()
            .map(|row| {
                Ok(Membership {
                    garden: GardenId(uuid(row, "garden_id")?),
                    user,
                    role: role(row, "role")?,
                    granted_by: opt_uuid(row, "granted_by")?.map(UserId),
                    granted_at: ts::decode(&row.try_get::<String, _>("granted_at")?)?,
                })
            })
            .collect()
    }

    pub async fn grant_membership(&self, membership: &Membership) -> Result<()> {
        sqlx::query(
            "INSERT INTO memberships (garden_id, user_id, role, granted_by, granted_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(garden_id, user_id) DO UPDATE SET role = excluded.role",
        )
        .bind(membership.garden.to_string())
        .bind(membership.user.to_string())
        .bind(membership.role.label())
        .bind(membership.granted_by.map(|u| u.to_string()))
        .bind(ts::encode(membership.granted_at))
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn revoke_membership(&self, garden: GardenId, user: UserId) -> Result<()> {
        sqlx::query("DELETE FROM memberships WHERE garden_id = ?1 AND user_id = ?2")
            .bind(garden.to_string())
            .bind(user.to_string())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Everyone with access to a garden, most privileged first.
    pub async fn members_of(&self, garden: GardenId) -> Result<Vec<MemberView>> {
        let rows = sqlx::query(
            "SELECT u.*, m.role AS m_role, m.granted_at AS m_granted_at,
                    m.granted_by AS m_granted_by
             FROM memberships m
             JOIN users u ON u.id = m.user_id
             WHERE m.garden_id = ?1",
        )
        .bind(garden.to_string())
        .fetch_all(&self.db)
        .await?;

        let mut members: Vec<MemberView> = rows
            .iter()
            .map(|row| {
                Ok(MemberView {
                    user: user_from_row(row)?,
                    role: role(row, "m_role")?,
                    granted_at: ts::decode(&row.try_get::<String, _>("m_granted_at")?)?,
                    granted_by: opt_uuid(row, "m_granted_by")?.map(UserId),
                })
            })
            .collect::<Result<_>>()?;

        members.sort_by(|a, b| {
            b.role
                .cmp(&a.role)
                .then_with(|| a.user.label().to_lowercase().cmp(&b.user.label().to_lowercase()))
        });
        Ok(members)
    }

    pub async fn role_of(&self, garden: GardenId, user: UserId) -> Result<Option<Role>> {
        let row = sqlx::query("SELECT role FROM memberships WHERE garden_id = ?1 AND user_id = ?2")
            .bind(garden.to_string())
            .bind(user.to_string())
            .fetch_optional(&self.db)
            .await?;
        row.as_ref().map(|r| role(r, "role")).transpose()
    }

    // --- Invitations --------------------------------------------------------------

    pub async fn create_invitation(&self, invitation: &Invitation) -> Result<()> {
        sqlx::query(
            "INSERT INTO invitations
                (id, garden_id, email, role, invited_by, digest, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(invitation.id.0.to_string())
        .bind(invitation.garden.to_string())
        .bind(invitation.email.as_str())
        .bind(invitation.role.label())
        .bind(invitation.invited_by.to_string())
        .bind(invitation.digest.as_str())
        .bind(ts::encode(invitation.created_at))
        .bind(ts::encode(invitation.expires_at))
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn find_invitation_by_token(&self, token: &SecretToken) -> Result<Option<Invitation>> {
        let row = sqlx::query("SELECT * FROM invitations WHERE digest = ?1")
            .bind(token.digest().as_str())
            .fetch_optional(&self.db)
            .await?;
        row.as_ref().map(invitation_from_row).transpose()
    }

    pub async fn invitations_for(&self, garden: GardenId) -> Result<Vec<Invitation>> {
        let rows = sqlx::query(
            "SELECT * FROM invitations WHERE garden_id = ?1 ORDER BY created_at DESC",
        )
        .bind(garden.to_string())
        .fetch_all(&self.db)
        .await?;
        rows.iter().map(invitation_from_row).collect()
    }

    pub async fn save_invitation(&self, invitation: &Invitation) -> Result<()> {
        sqlx::query(
            "UPDATE invitations
             SET accepted_at = ?1, accepted_by = ?2, revoked_at = ?3
             WHERE id = ?4",
        )
        .bind(ts::encode_opt(invitation.accepted_at))
        .bind(invitation.accepted_by.map(|u| u.to_string()))
        .bind(ts::encode_opt(invitation.revoked_at))
        .bind(invitation.id.0.to_string())
        .execute(&self.db)
        .await?;
        Ok(())
    }
}

fn invitation_from_row(row: &SqliteRow) -> Result<Invitation> {
    let email: String = row.try_get("email")?;
    Ok(Invitation {
        id: InvitationId(uuid(row, "id")?),
        garden: GardenId(uuid(row, "garden_id")?),
        email: EmailAddress::parse(&email)
            .map_err(|e| StoreError::Corrupt(format!("invite email {email:?}: {e}")))?,
        role: role(row, "role")?,
        invited_by: UserId(uuid(row, "invited_by")?),
        digest: gardyn_auth::TokenDigest::from_stored(row.try_get::<String, _>("digest")?),
        created_at: ts::decode(&row.try_get::<String, _>("created_at")?)?,
        expires_at: ts::decode(&row.try_get::<String, _>("expires_at")?)?,
        accepted_at: ts::decode_opt(row.try_get("accepted_at")?)?,
        accepted_by: opt_uuid(row, "accepted_by")?.map(UserId),
        revoked_at: ts::decode_opt(row.try_get("revoked_at")?)?,
    })
}

/// A real Argon2 hash of a fixed string, so the miss path costs what the hit path
/// costs. Built once per call; Argon2 dominates the timing either way.
fn decoy_digest() -> PasswordDigest {
    PasswordDigest::from_stored(
        "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$\
         8pu2GRPZ8dOZFqJHTfj5ZfLDF0kBqYCn0hMJPXHqLDo",
    )
}
