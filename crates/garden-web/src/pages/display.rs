//! A compact read-only status feed for a counter display.
//!
//! Written for an ESP32 with a small LCD and a few kilobytes of RAM to spare, which
//! shapes every decision here:
//!
//! - **One request, no session.** The device holds an unguessable URL, exactly like the
//!   calendar feed. It is scoped to one garden and to reading, because a display on a
//!   worktop is the least physically secure thing in the system.
//! - **Garden-level tasks only.** A counter display cannot usefully say "thin planting
//!   7" — there is no slot map on it and nobody is standing at it holding the tower.
//!   Filtered by target rather than by mode, so the same firmware works against an
//!   advanced garden and simply sees fewer items.
//! - **Numbers already reduced.** Percentages, not millimetres and tank geometry; a
//!   severity rank as an integer, so the firmware can threshold without comparing
//!   strings.
//! - **Absent rather than null** wherever a sensor is missing, to keep the document
//!   small and the parser simple.
//!
//! The rules are not duplicated here. The display shows what the same engine already
//! decided, which is the entire reason it can be this small.

use crate::app::AppState;
use crate::error::AppError;
use axum::extract::{Path, State};
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::get, routing::post};
use garden_auth::SecretToken;
use garden_core::{Severity, Target};
use serde::Serialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/display/{token}/status.json", get(status))
        .route("/display/{token}/done", post(done))
}

/// Everything a counter display needs, and nothing else.
#[derive(Debug, Serialize, PartialEq)]
pub struct DisplayStatus {
    pub garden: String,
    /// Server time, unix seconds. The device can show its own staleness if the
    /// network drops without needing a clock of its own.
    pub generated_at: i64,
    /// Seconds since the newest sensor reading. Absent means nothing has ever
    /// reported, which is a different thing from "reported a long time ago" and
    /// should look different on the screen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reading_age_s: Option<i64>,
    /// Tank fill, 0-100. The one gauge worth a dial.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub water_pct: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub air_c: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub humidity_pct: Option<u8>,
    /// Outstanding garden-level work, most severe first.
    pub tasks: Vec<DisplayTask>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct DisplayTask {
    /// What the button sends back to clear this task. Opaque to the firmware: copy
    /// it, do not construct it. Keys carry a discriminator for broad kinds, so
    /// rebuilding one from `kind` alone would be right most of the time and wrong
    /// exactly where two concerns share a kind.
    pub key: String,
    /// Stable key, matching what the database stores. Firmware switches on this.
    pub kind: String,
    /// The same thing, short enough for a 128-pixel line and distinct from its
    /// neighbours. Draw this; `kind` is for logic.
    pub title: String,
    /// A short name for a picture of this task — "droplet", "sponge", "scissors".
    /// Shared with the notification tags, because both are asking the same question.
    pub icon: String,
    /// 0 Info … 4 Critical. An integer so a microcontroller can threshold on it
    /// without string comparison, and so a severity added later sorts sensibly
    /// instead of landing in an unknown branch.
    pub level: u8,
    pub severity: String,
    /// The rule's own reasoning, verbatim. Already written to be read by a person
    /// glancing at it, which is the same job the LCD has.
    pub why: String,
    /// Days past due. Negative means not yet due; absent tasks are simply not listed.
    pub overdue_days: i64,
}

/// A title short enough to survive a 128-pixel line, and distinct from every other.
///
/// The panel fits fifteen characters beside the icon, and the stored labels do not.
/// "add water conditioner" clips to "ADD WATER CONDI", which is not merely truncated
/// — it is confusable with "ADD WATER", a different job, read across a kitchen. Two
/// tasks that look alike at a glance are worse than one that is abbreviated.
///
/// Sent in the payload rather than mapped in firmware, so the device needs no table
/// and a wording fix does not need a reflash.
pub fn panel_title(kind: &str) -> &'static str {
    match kind {
        "add water" => "TOP UP WATER",
        "add plant food" => "PLANT FOOD",
        "add water conditioner" => "CONDITIONER",
        "prune roots" => "CHECK ROOTS",
        "refresh tank" => "REFRESH TANK",
        "deep clean" => "DEEP CLEAN",
        // Honest, and still actionable: the reason line below says what to look at.
        _ => "CHECK GARDEN",
    }
}

/// Characters that fit beside the icon on the panel.
pub const PANEL_CELLS: usize = 15;

fn level(severity: Severity) -> u8 {
    match severity {
        Severity::Info => 0,
        Severity::Advisory => 1,
        Severity::Important => 2,
        Severity::Urgent => 3,
        Severity::Critical => 4,
    }
}

/// Resolve a display secret to the one garden it may act on.
///
/// An unparseable token is refused identically to a valid one that matches nothing,
/// so the shape of the secret is not a probe for which half is wrong.
async fn authorize(state: &AppState, token: &str) -> Result<garden_core::Garden, AppError> {
    let token = SecretToken::from_client(token).ok_or(AppError::NotFound)?;
    let garden_id = state
        .store
        .garden_for_display_token(&token)
        .await?
        .ok_or(AppError::NotFound)?;
    state.store.find_garden(garden_id).await?.ok_or(AppError::NotFound)
}

/// What the button sends.
#[derive(serde::Deserialize)]
pub struct DoneRequest {
    /// The `key` from a task in the last status response, copied verbatim.
    key: String,
}

/// Clear a task from the screen, and close the loop behind it.
///
/// The same path a browser takes, so the tank event is recorded and the rule stands
/// down — a display that only blanked its own screen would show the task again within
/// five minutes and look broken.
///
/// **This is the one thing a display secret can write.** It is scoped to garden-level
/// tasks on one garden: a device on a worktop must not be able to complete per-plant
/// work it cannot see, change a setting, or report telemetry. The honest cost of the
/// button is that a leaked URL can mark a tank as fed when it was not, which
/// under-feeds a garden until someone notices. That is the trade the button is worth;
/// everything with a larger blast radius stays behind a session.
async fn done(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Json(request): Json<DoneRequest>,
) -> Result<Response, AppError> {
    let garden = authorize(&state, &token).await?;
    let now = state.now();
    let key = garden_core::TaskKey(request.key);

    let Some(task) = state.store.find_task(garden.id, &key).await? else {
        return Err(AppError::NotFound);
    };
    // A display shows garden-level work and may therefore only complete garden-level
    // work. Checked against the stored row rather than against what was sent, so a
    // crafted key cannot reach a planting.
    if task.target != Target::Garden.to_string() {
        return Err(AppError::NotFound);
    }

    // Attributed to nobody: there is a button press behind this, not a person.
    crate::pages::tasks::complete_and_close_the_loop(&state, garden.id, &task, None, now).await?;
    state
        .store
        .log_event(
            garden.id,
            "display.completed",
            Some(&format!("{} cleared from the counter display", task.kind)),
            None,
            now,
        )
        .await?;

    // The refreshed screen, in the same round trip. A microcontroller that had to
    // poll again would show the cleared task for however long its interval is.
    render(&state, &garden, now).await
}

async fn status(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, AppError> {
    let garden = authorize(&state, &token).await?;
    let now = state.now();
    render(&state, &garden, now).await
}

async fn render(
    state: &AppState,
    garden: &garden_core::Garden,
    now: jiff::Timestamp,
) -> Result<Response, AppError> {
    let snapshot = crate::state::build(&state.store, garden, now).await?;

    // Read from the tasks table, not from a fresh evaluation. The display must agree
    // with the web UI and with whatever was pushed to the phone; re-deriving here
    // would let a task be shown as outstanding on the counter seconds after it was
    // marked done on a phone.
    let mut tasks: Vec<DisplayTask> = state
        .store
        .tasks_for(garden.id)
        .await?
        .into_iter()
        .filter(|t| t.is_actionable(now))
        .filter(|t| t.target == Target::Garden.to_string())
        .map(|t| DisplayTask {
            key: t.key.0.clone(),
            icon: garden_notify::message::tag_for(
                garden_core::TaskKind::from_label(&t.kind).unwrap_or(garden_core::TaskKind::Inspect),
            )
            .to_string(),
            title: panel_title(&t.kind).to_string(),
            level: level(t.severity),
            severity: t.severity.label().to_string(),
            why: t.rationale.clone(),
            overdue_days: (now.as_second() - t.due_at.as_second()) / 86_400,
            kind: t.kind,
        })
        .collect();
    tasks.sort_by(|a, b| b.level.cmp(&a.level).then(a.overdue_days.cmp(&b.overdue_days)));

    let fill = snapshot.tank_geometry.fill_fraction(snapshot.tank.volume_l);
    let body = DisplayStatus {
        garden: garden.name.clone(),
        generated_at: now.as_second(),
        reading_age_s: (!snapshot.capabilities.is_empty())
            .then(|| now.as_second() - snapshot.sensors.at.as_second()),
        water_pct: snapshot
            .capabilities
            .contains(garden_core::Capability::WaterLevel)
            .then(|| (fill * 100.0).round().clamp(0.0, 100.0) as u8),
        air_c: snapshot.sensors.air_temp_c,
        humidity_pct: snapshot
            .sensors
            .humidity_pct
            .map(|h| h.round().clamp(0.0, 100.0) as u8),
        tasks,
    };

    // No caching. A display polling every few minutes must not be handed a task list
    // that was already stale when it was stored.
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(body),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_panel_title_fits_and_none_is_confusable_with_another() {
        // Found by building the display mock rather than by reading the code: the
        // stored label "add water conditioner" clips to "ADD WATER CONDI", which is
        // not just truncated but ambiguous with "ADD WATER" — a different job.
        let kinds = [
            "add water",
            "add plant food",
            "add water conditioner",
            "prune roots",
            "refresh tank",
            "deep clean",
        ];
        let titles: Vec<&str> = kinds.iter().map(|k| panel_title(k)).collect();

        for (kind, title) in kinds.iter().zip(&titles) {
            assert!(
                title.chars().count() <= PANEL_CELLS,
                "'{kind}' -> '{title}' is {} cells, over {PANEL_CELLS}",
                title.chars().count()
            );
            assert_ne!(*title, panel_title("?"), "'{kind}' fell through to the fallback");
        }
        // Distinct once clipped, which is the property that actually matters — two
        // titles differing only past the cut are identical on the panel.
        for i in 0..titles.len() {
            for j in (i + 1)..titles.len() {
                assert_ne!(titles[i], titles[j], "{} and {} read alike", kinds[i], kinds[j]);
                assert!(
                    !titles[i].starts_with(titles[j]) && !titles[j].starts_with(titles[i]),
                    "'{}' and '{}' are confusable at a glance",
                    titles[i],
                    titles[j]
                );
            }
        }
    }

    #[test]
    fn severity_ranks_are_ordered_and_stable() {
        // The firmware thresholds on this number, so the order is a contract. A
        // reshuffle would silently change which tasks light the red LED.
        assert_eq!(level(Severity::Info), 0);
        assert_eq!(level(Severity::Critical), 4);
        let ranks: Vec<u8> = [
            Severity::Info,
            Severity::Advisory,
            Severity::Important,
            Severity::Urgent,
            Severity::Critical,
        ]
        .into_iter()
        .map(level)
        .collect();
        assert!(ranks.windows(2).all(|w| w[0] < w[1]), "{ranks:?}");
    }

    #[test]
    fn absent_sensors_are_omitted_rather_than_sent_as_null() {
        // Every byte matters on a device parsing this with a few KB of headroom, and
        // a missing key is easier to branch on than a null.
        let json = serde_json::to_string(&DisplayStatus {
            garden: "Kitchen".into(),
            generated_at: 1_700_000_000,
            reading_age_s: None,
            water_pct: None,
            air_c: None,
            humidity_pct: None,
            tasks: Vec::new(),
        })
        .unwrap();

        assert!(!json.contains("null"), "{json}");
        assert!(!json.contains("water_pct"), "{json}");
        assert!(json.contains("\"tasks\":[]"), "an empty list must still be sent: {json}");
    }

    #[test]
    fn a_present_but_zero_reading_is_still_sent() {
        // The failure the `skip_serializing_if` above invites: an empty tank is 0%,
        // which is the single most important thing this endpoint can say, and it must
        // not be mistaken for an absent sensor and dropped.
        let json = serde_json::to_string(&DisplayStatus {
            garden: "Kitchen".into(),
            generated_at: 1_700_000_000,
            reading_age_s: Some(0),
            water_pct: Some(0),
            air_c: Some(0.0),
            humidity_pct: Some(0),
            tasks: Vec::new(),
        })
        .unwrap();

        assert!(json.contains("\"water_pct\":0"), "{json}");
        assert!(json.contains("\"reading_age_s\":0"), "{json}");
    }

    #[test]
    fn the_document_stays_small_enough_for_a_microcontroller() {
        // A worst-case garden: every garden-level task outstanding at once. If this
        // does not fit comfortably in a static buffer, the firmware needs streaming
        // parsing, and the whole point of this endpoint is that it does not.
        let tasks: Vec<DisplayTask> = (0..8)
            .map(|i| DisplayTask {
                key: "addconditioner:garden".into(),
                kind: "add water conditioner".into(),
                title: "CONDITIONER".into(),
                icon: "test_tube".into(),
                level: 3,
                severity: "urgent".into(),
                why: "tank at 22% (3.4 L), using 0.50 L/day — reserve reached in 1.8 days"
                    .into(),
                overdue_days: i,
            })
            .collect();
        let json = serde_json::to_string(&DisplayStatus {
            garden: "SirGrowsAlot".into(),
            generated_at: 1_700_000_000,
            reading_age_s: Some(42),
            water_pct: Some(22),
            air_c: Some(21.4),
            humidity_pct: Some(54),
            tasks,
        })
        .unwrap();

        assert!(json.len() < 2048, "{} bytes is too big for a 2 KB buffer", json.len());
    }
}
