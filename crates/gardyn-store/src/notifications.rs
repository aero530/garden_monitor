//! Notification preferences and delivery history.

use crate::{Result, Store, StoreError, ts};
use gardyn_auth::{SecretToken, UserId};
use gardyn_core::{GardenId, Severity, TaskKey};
use gardyn_notify::{LastNotified, QuietHours};
use jiff::Timestamp;
use sqlx::Row;
use uuid::Uuid;

/// How someone wants to be told.
#[derive(Debug, Clone, PartialEq)]
pub struct NotificationPrefs {
    pub user: UserId,
    /// ntfy topic. `None` means the phone app is not set up, so no push.
    pub ntfy_topic: Option<String>,
    pub email_enabled: bool,
    pub quiet: QuietHours,
    /// Offset from UTC, in minutes. Quiet hours are meaningless without it.
    pub utc_offset_minutes: i32,
    pub has_calendar_feed: bool,
}

impl NotificationPrefs {
    /// Sensible defaults for someone who has never opened the settings page: quiet
    /// overnight, and silent everywhere until they opt in.
    pub fn default_for(user: UserId) -> Self {
        Self {
            user,
            ntfy_topic: None,
            email_enabled: false,
            quiet: QuietHours::default(),
            utc_offset_minutes: 0,
            has_calendar_feed: false,
        }
    }

    /// Local hour for this person at `now`.
    pub fn local_hour(&self, now: Timestamp) -> u8 {
        let shifted = now.as_second() + i64::from(self.utc_offset_minutes) * 60;
        // `rem_euclid` so a negative offset before midnight UTC does not produce a
        // negative hour, which would make every quiet-hours check nonsense.
        ((shifted.rem_euclid(86_400)) / 3600) as u8
    }

    pub fn wants_anything(&self) -> bool {
        self.ntfy_topic.is_some() || self.email_enabled
    }
}

fn severity_from_str(s: &str) -> Result<Severity> {
    Ok(match s {
        "info" => Severity::Info,
        "advisory" => Severity::Advisory,
        "important" => Severity::Important,
        "urgent" => Severity::Urgent,
        "critical" => Severity::Critical,
        other => return Err(StoreError::Corrupt(format!("severity {other:?}"))),
    })
}

impl Store {
    pub async fn notification_prefs(&self, user: UserId) -> Result<NotificationPrefs> {
        let row = sqlx::query("SELECT * FROM notification_prefs WHERE user_id = ?1")
            .bind(user.to_string())
            .fetch_optional(&self.db)
            .await?;

        let Some(row) = row else {
            return Ok(NotificationPrefs::default_for(user));
        };
        let digest: Option<String> = row.try_get("calendar_digest")?;

        Ok(NotificationPrefs {
            user,
            ntfy_topic: row
                .try_get::<Option<String>, _>("ntfy_topic")?
                .filter(|t| !t.trim().is_empty()),
            email_enabled: row.try_get::<i64, _>("email_enabled")? != 0,
            quiet: QuietHours {
                from_hour: row.try_get::<i64, _>("quiet_from_hour")?.clamp(0, 23) as u8,
                to_hour: row.try_get::<i64, _>("quiet_to_hour")?.clamp(0, 23) as u8,
            },
            utc_offset_minutes: row.try_get::<i64, _>("utc_offset_minutes")?.clamp(-840, 840) as i32,
            has_calendar_feed: digest.is_some(),
        })
    }

    pub async fn save_notification_prefs(&self, prefs: &NotificationPrefs) -> Result<()> {
        sqlx::query(
            "INSERT INTO notification_prefs
                (user_id, ntfy_topic, email_enabled, quiet_from_hour, quiet_to_hour,
                 utc_offset_minutes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(user_id) DO UPDATE SET
                ntfy_topic = excluded.ntfy_topic,
                email_enabled = excluded.email_enabled,
                quiet_from_hour = excluded.quiet_from_hour,
                quiet_to_hour = excluded.quiet_to_hour,
                utc_offset_minutes = excluded.utc_offset_minutes",
        )
        .bind(prefs.user.to_string())
        .bind(prefs.ntfy_topic.as_deref().map(str::trim).filter(|t| !t.is_empty()))
        .bind(i64::from(prefs.email_enabled))
        .bind(i64::from(prefs.quiet.from_hour))
        .bind(i64::from(prefs.quiet.to_hour))
        .bind(i64::from(prefs.utc_offset_minutes))
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Mint a calendar feed secret, replacing any existing one.
    ///
    /// Returned once. Only the digest is stored, so re-issuing is the only way to
    /// recover a lost URL — which is also how you revoke one.
    pub async fn issue_calendar_feed(&self, user: UserId) -> Result<SecretToken> {
        let token = SecretToken::generate();
        sqlx::query(
            "INSERT INTO notification_prefs (user_id, calendar_digest)
             VALUES (?1, ?2)
             ON CONFLICT(user_id) DO UPDATE SET calendar_digest = excluded.calendar_digest",
        )
        .bind(user.to_string())
        .bind(token.digest().as_str())
        .execute(&self.db)
        .await?;
        Ok(token)
    }

    /// Resolve a calendar feed URL back to its owner.
    ///
    /// This is the one place a bearer secret grants read access without a session, so
    /// it is scoped to exactly one thing: the person's own task feed.
    pub async fn user_for_calendar_token(&self, token: &SecretToken) -> Result<Option<UserId>> {
        let row = sqlx::query("SELECT user_id FROM notification_prefs WHERE calendar_digest = ?1")
            .bind(token.digest().as_str())
            .fetch_optional(&self.db)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let raw: String = row.try_get("user_id")?;
        Ok(Some(UserId(Uuid::parse_str(&raw).map_err(|e| {
            StoreError::Corrupt(format!("user id: {e}"))
        })?)))
    }

    pub async fn revoke_calendar_feed(&self, user: UserId) -> Result<()> {
        sqlx::query("UPDATE notification_prefs SET calendar_digest = NULL WHERE user_id = ?1")
            .bind(user.to_string())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    // --- Delivery history --------------------------------------------------------

    pub async fn last_notified(
        &self,
        garden: GardenId,
        user: UserId,
        task: &TaskKey,
    ) -> Result<Option<LastNotified>> {
        let row = sqlx::query(
            "SELECT severity, sent_at FROM notifications
             WHERE garden_id = ?1 AND user_id = ?2 AND task_key = ?3
             ORDER BY sent_at DESC LIMIT 1",
        )
        .bind(garden.to_string())
        .bind(user.to_string())
        .bind(&task.0)
        .fetch_optional(&self.db)
        .await?;

        let Some(row) = row else { return Ok(None) };
        Ok(Some(LastNotified {
            at: ts::decode(&row.try_get::<String, _>("sent_at")?)?,
            severity: severity_from_str(&row.try_get::<String, _>("severity")?)?,
        }))
    }

    pub async fn record_notification(
        &self,
        garden: GardenId,
        user: UserId,
        task: &TaskKey,
        severity: Severity,
        channels: &str,
        at: Timestamp,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO notifications (id, garden_id, user_id, task_key, severity, channels, sent_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(garden.to_string())
        .bind(user.to_string())
        .bind(&task.0)
        .bind(severity.label())
        .bind(channels)
        .bind(ts::encode(at))
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Forget delivery history for tasks that no longer exist.
    ///
    /// Without this, a task that resolves and later recurs would be treated as
    /// "already sent" and stay silent.
    pub async fn prune_notifications(&self, garden: GardenId) -> Result<u64> {
        let result = sqlx::query(
            // `__`-prefixed keys are the dispatcher's own bookkeeping (the daily
            // brief), not real tasks. They have no row in `tasks` to match, so they
            // must be spared or the brief would re-fire on every sweep.
            "DELETE FROM notifications
             WHERE garden_id = ?1
               AND substr(task_key, 1, 2) <> '__'
               AND task_key NOT IN (SELECT task_key FROM tasks WHERE garden_id = ?1)",
        )
        .bind(garden.to_string())
        .execute(&self.db)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn notification_count(&self, garden: GardenId) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM notifications WHERE garden_id = ?1")
            .bind(garden.to_string())
            .fetch_one(&self.db)
            .await?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gardyn_core::{Target, TaskKind};

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn key() -> TaskKey {
        TaskKey::new(TaskKind::AddWater, Target::Garden)
    }

    async fn fixture() -> (Store, GardenId, UserId) {
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
        (store, garden.id, user.id)
    }

    #[tokio::test]
    async fn someone_who_never_opened_settings_gets_nothing() {
        // Silence by default. Opting in is a deliberate act.
        let (store, _, user) = fixture().await;
        let prefs = store.notification_prefs(user).await.unwrap();
        assert!(!prefs.wants_anything());
        assert!(prefs.ntfy_topic.is_none());
        assert!(!prefs.email_enabled);
    }

    #[tokio::test]
    async fn preferences_round_trip() {
        let (store, _, user) = fixture().await;
        let mut prefs = NotificationPrefs::default_for(user);
        prefs.ntfy_topic = Some("gardyn-phil-a8f3".into());
        prefs.email_enabled = true;
        prefs.quiet = QuietHours {
            from_hour: 22,
            to_hour: 6,
        };
        prefs.utc_offset_minutes = -360;
        store.save_notification_prefs(&prefs).await.unwrap();

        let back = store.notification_prefs(user).await.unwrap();
        assert_eq!(back.ntfy_topic.as_deref(), Some("gardyn-phil-a8f3"));
        assert!(back.email_enabled);
        assert_eq!(back.quiet.from_hour, 22);
        assert_eq!(back.utc_offset_minutes, -360);
        assert!(back.wants_anything());
    }

    #[tokio::test]
    async fn a_blank_topic_is_stored_as_no_topic() {
        // Otherwise an empty string would look like a configured topic and every push
        // would fail against a nonexistent one.
        let (store, _, user) = fixture().await;
        let mut prefs = NotificationPrefs::default_for(user);
        prefs.ntfy_topic = Some("   ".into());
        store.save_notification_prefs(&prefs).await.unwrap();
        assert!(store.notification_prefs(user).await.unwrap().ntfy_topic.is_none());
    }

    #[test]
    fn local_hour_handles_a_negative_offset_across_midnight() {
        // 02:00 UTC at UTC-7 is 19:00 the previous day. A naive division gives a
        // negative hour and quiet-hours checks stop working.
        let mut prefs = NotificationPrefs::default_for(UserId::new());
        prefs.utc_offset_minutes = -420;
        let two_am_utc = Timestamp::from_second(2 * 3600).unwrap();
        assert_eq!(prefs.local_hour(two_am_utc), 19);
    }

    #[test]
    fn local_hour_handles_a_positive_offset() {
        let mut prefs = NotificationPrefs::default_for(UserId::new());
        prefs.utc_offset_minutes = 330; // UTC+5:30
        let midnight_utc = Timestamp::from_second(0).unwrap();
        assert_eq!(prefs.local_hour(midnight_utc), 5);
    }

    #[tokio::test]
    async fn a_calendar_feed_resolves_to_its_owner_and_nobody_else() {
        let (store, _, user) = fixture().await;
        let token = store.issue_calendar_feed(user).await.unwrap();

        assert_eq!(
            store.user_for_calendar_token(&token).await.unwrap(),
            Some(user)
        );
        let forged = SecretToken::generate();
        assert!(store.user_for_calendar_token(&forged).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn re_issuing_a_feed_invalidates_the_old_url() {
        let (store, _, user) = fixture().await;
        let first = store.issue_calendar_feed(user).await.unwrap();
        let second = store.issue_calendar_feed(user).await.unwrap();

        assert!(store.user_for_calendar_token(&first).await.unwrap().is_none());
        assert_eq!(
            store.user_for_calendar_token(&second).await.unwrap(),
            Some(user)
        );
    }

    #[tokio::test]
    async fn revoking_a_feed_stops_it_resolving() {
        let (store, _, user) = fixture().await;
        let token = store.issue_calendar_feed(user).await.unwrap();
        store.revoke_calendar_feed(user).await.unwrap();
        assert!(store.user_for_calendar_token(&token).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn issuing_a_feed_does_not_clobber_existing_preferences() {
        // Both write to the same row through different upserts.
        let (store, _, user) = fixture().await;
        let mut prefs = NotificationPrefs::default_for(user);
        prefs.ntfy_topic = Some("gardyn-phil".into());
        store.save_notification_prefs(&prefs).await.unwrap();

        store.issue_calendar_feed(user).await.unwrap();

        let back = store.notification_prefs(user).await.unwrap();
        assert_eq!(back.ntfy_topic.as_deref(), Some("gardyn-phil"));
        assert!(back.has_calendar_feed);
    }

    #[tokio::test]
    async fn delivery_history_records_what_was_sent() {
        let (store, garden, user) = fixture().await;
        assert!(store.last_notified(garden, user, &key()).await.unwrap().is_none());

        store
            .record_notification(garden, user, &key(), Severity::Urgent, "push,email", t0())
            .await
            .unwrap();

        let last = store.last_notified(garden, user, &key()).await.unwrap().unwrap();
        assert_eq!(last.severity, Severity::Urgent);
        assert_eq!(last.at, t0());
    }

    #[tokio::test]
    async fn the_most_recent_send_is_the_one_that_counts() {
        let (store, garden, user) = fixture().await;
        store
            .record_notification(garden, user, &key(), Severity::Important, "push", t0())
            .await
            .unwrap();
        let later = gardyn_core::time::add_days(t0(), 1.0);
        store
            .record_notification(garden, user, &key(), Severity::Critical, "push", later)
            .await
            .unwrap();

        let last = store.last_notified(garden, user, &key()).await.unwrap().unwrap();
        assert_eq!(last.severity, Severity::Critical);
    }

    #[tokio::test]
    async fn one_persons_history_is_not_anothers() {
        // Two people sharing a garden are notified independently.
        let (store, garden, phil) = fixture().await;
        let sam = store
            .create_user(
                gardyn_auth::EmailAddress::parse("sam@example.com").unwrap(),
                "Sam",
                "a long enough password",
                t0(),
            )
            .await
            .unwrap();

        store
            .record_notification(garden, phil, &key(), Severity::Urgent, "push", t0())
            .await
            .unwrap();

        assert!(store.last_notified(garden, phil, &key()).await.unwrap().is_some());
        assert!(store.last_notified(garden, sam.id, &key()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn history_for_a_resolved_task_is_pruned_so_a_recurrence_is_announced() {
        // The bug this prevents: a task resolves, comes back a month later, and stays
        // silent because the dispatcher still thinks it was already sent.
        let (store, garden, user) = fixture().await;
        store
            .record_notification(garden, user, &key(), Severity::Urgent, "push", t0())
            .await
            .unwrap();
        assert_eq!(store.notification_count(garden).await.unwrap(), 1);

        // No matching row in `tasks`, so the condition has resolved.
        let removed = store.prune_notifications(garden).await.unwrap();
        assert_eq!(removed, 1);
        assert!(store.last_notified(garden, user, &key()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn deleting_a_garden_takes_its_notification_history() {
        let (store, garden, user) = fixture().await;
        store
            .record_notification(garden, user, &key(), Severity::Urgent, "push", t0())
            .await
            .unwrap();
        store.delete_garden(garden).await.unwrap();
        assert_eq!(store.notification_count(garden).await.unwrap(), 0);
    }
}
