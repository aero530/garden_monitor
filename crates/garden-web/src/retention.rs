//! Throwing away what is no longer worth keeping.
//!
//! Four pruning helpers existed in `garden-store` for a long time, tested, and called
//! by nothing. Meanwhile readings accumulate at roughly half a million rows a year per
//! garden and frames at ~8,700 files, and DEPLOYMENT.md's advice for a full disk was to
//! turn the camera down. This is the job that should have been running all along.
//!
//! Daily, not on the five-minute dispatcher tick. Retention moves on the scale of
//! months; running it 288 times a day would be the same work over and over for no
//! benefit, and it would put a `DELETE` scan in front of every notification sweep.

use crate::app::AppState;
use garden_store::settings::DEFAULT_FRAME_RETENTION_DAYS;
use std::time::Duration;

/// How often the sweep runs.
pub const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Delay before the first run, so a restart loop cannot turn into a delete loop and so
/// startup logs are not interleaved with pruning.
const FIRST_RUN_DELAY: Duration = Duration::from_secs(120);

/// How long raw sensor readings are kept.
///
/// Ninety days of full resolution is plenty to fit the growth and consumption curves;
/// older than that belongs in a rollup, which does not exist yet. Overridable because
/// somebody investigating a slow drift may want a year.
fn reading_days() -> f64 {
    env_days(
        "GARDEN_RETAIN_READING_DAYS",
        garden_store::readings::DEFAULT_RETENTION_DAYS,
    )
}

/// Per-slot vision metrics. Far smaller than readings — a few thousand rows a year —
/// so a full season of canopy history is cheap to keep.
fn metric_days() -> f64 {
    env_days(
        "GARDEN_RETAIN_METRIC_DAYS",
        garden_store::vision::DEFAULT_RETENTION_DAYS,
    )
}

fn env_days(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d >= 1.0)
        .unwrap_or(default)
}

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_RUN_DELAY).await;
        loop {
            if let Err(error) = sweep(&state).await {
                tracing::error!(%error, "retention sweep failed");
            }
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

/// One pass. Returns what it removed, so the test can assert on it.
pub async fn sweep(state: &AppState) -> Result<Removed, garden_store::StoreError> {
    let now = state.now();

    // Sessions are global rather than per-garden, and an expired one is dead weight in
    // a table every request touches.
    let mut removed = Removed {
        sessions: state.store.purge_expired_sessions(now).await?,
        metrics: state
            .store
            .prune_slot_metrics(garden_core::time::add_days(now, -metric_days()))
            .await?,
        ..Removed::default()
    };

    for garden in state.store.all_gardens().await? {
        // Each garden's own retention. Read per garden rather than hoisted, because
        // that is the whole point of the setting.
        let keep = state
            .store
            .frame_retention_days(garden.id)
            .await
            .unwrap_or(DEFAULT_FRAME_RETENTION_DAYS);

        match state.store.prune_frames(garden.id, keep as f64, now).await {
            Ok(n) => removed.frames += n,
            // One garden's failure must not stop the others being tidied.
            Err(error) => tracing::warn!(garden = %garden.id, %error, "could not prune frames"),
        }
        match state
            .store
            .prune_readings(garden.id, reading_days(), now)
            .await
        {
            Ok(n) => removed.readings += n,
            Err(error) => tracing::warn!(garden = %garden.id, %error, "could not prune readings"),
        }
    }

    if removed.any() {
        tracing::info!(
            frames = removed.frames,
            readings = removed.readings,
            metrics = removed.metrics,
            sessions = removed.sessions,
            "retention sweep removed rows"
        );
    }
    Ok(removed)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Removed {
    pub frames: u64,
    pub readings: u64,
    pub metrics: u64,
    pub sessions: u64,
}

impl Removed {
    pub fn any(&self) -> bool {
        self.frames + self.readings + self.metrics + self.sessions > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Config;
    use garden_auth::EmailAddress;
    use garden_core::{DeviceModel, SensorSnapshot, Timestamp, time::add_days};
    use garden_store::Store;
    use garden_store::frames::{FrameSource, NewFrame};

    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
        0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
        0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
        0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
        0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// A state whose clock is pinned, so "old" is deterministic.
    async fn fixture(now: Timestamp) -> (AppState, garden_core::GardenId) {
        let store = Store::in_memory().await.unwrap();
        let user = store
            .create_user(
                EmailAddress::parse("phil@example.com").unwrap(),
                "Phil",
                "a long enough password",
                now,
            )
            .await
            .unwrap();
        let garden = store
            .create_garden("Kitchen", DeviceModel::Studio2, "UTC", user.id, now)
            .await
            .unwrap();
        let state = AppState::new(
            store,
            Config {
                secure_cookies: false,
                base_url: "http://localhost:8080".into(),
                agent_token: None,
            },
        )
        .with_clock_at(now);
        (state, garden.id)
    }

    async fn frame_at(state: &AppState, garden: garden_core::GardenId, at: Timestamp) {
        state
            .store
            .put_frame(NewFrame {
                garden,
                captured_at: at,
                width: 1,
                height: 1,
                light_duty_milli: Some(800),
                comparable: true,
                source: FrameSource::Agent,
                bytes: PNG,
            })
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn an_empty_server_removes_nothing_and_says_nothing() {
        let now = Timestamp::from_second(1_800_000_000).unwrap();
        let (state, _) = fixture(now).await;
        assert_eq!(sweep(&state).await.unwrap(), Removed::default());
    }

    #[tokio::test]
    async fn frames_past_the_gardens_own_window_go() {
        let now = Timestamp::from_second(1_800_000_000).unwrap();
        let (state, garden) = fixture(now).await;

        frame_at(&state, garden, add_days(now, -120.0)).await;
        frame_at(&state, garden, add_days(now, -100.0)).await;
        frame_at(&state, garden, add_days(now, -10.0)).await;

        // Default 90 days: the two old ones go.
        let removed = sweep(&state).await.unwrap();
        assert_eq!(removed.frames, 2);
        assert_eq!(state.store.frame_storage(garden).await.unwrap().count, 1);
    }

    #[tokio::test]
    async fn the_gardens_own_setting_is_what_is_honoured() {
        // The point of making it per-garden.
        let now = Timestamp::from_second(1_800_000_000).unwrap();
        let (state, garden) = fixture(now).await;
        state
            .store
            .set_frame_retention_days(garden, 30, now)
            .await
            .unwrap();

        frame_at(&state, garden, add_days(now, -60.0)).await;
        frame_at(&state, garden, add_days(now, -10.0)).await;

        assert_eq!(sweep(&state).await.unwrap().frames, 1);
    }

    #[tokio::test]
    async fn old_readings_go_and_recent_ones_stay() {
        let now = Timestamp::from_second(1_800_000_000).unwrap();
        let (state, garden) = fixture(now).await;

        for days in [400.0, 200.0, 5.0] {
            let mut sensors = SensorSnapshot::empty(add_days(now, -days));
            sensors.air_temp_c = Some(21.0);
            state.store.record_reading(garden, &sensors, None).await.unwrap();
        }

        let removed = sweep(&state).await.unwrap();
        assert_eq!(removed.readings, 2, "90-day default");
        assert_eq!(state.store.reading_count(garden).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn expired_sessions_are_purged() {
        let now = Timestamp::from_second(1_800_000_000).unwrap();
        let (state, _) = fixture(now).await;
        let user = state
            .store
            .find_user_by_email(&EmailAddress::parse("phil@example.com").unwrap())
            .await
            .unwrap()
            .unwrap();

        // Issued long enough ago that its 30-day life is over.
        state
            .store
            .open_session(user.id, add_days(now, -400.0), None)
            .await
            .unwrap();

        assert_eq!(sweep(&state).await.unwrap().sessions, 1);
    }

    #[tokio::test]
    async fn a_second_sweep_finds_nothing_left_to_do() {
        // Idempotence matters: this runs every day forever, and a sweep that kept
        // finding work would mean it was not actually deleting anything.
        let now = Timestamp::from_second(1_800_000_000).unwrap();
        let (state, garden) = fixture(now).await;
        frame_at(&state, garden, add_days(now, -120.0)).await;

        assert!(sweep(&state).await.unwrap().any());
        assert_eq!(sweep(&state).await.unwrap(), Removed::default());
    }
}
