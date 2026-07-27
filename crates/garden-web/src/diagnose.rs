//! Asking a local vision model what it makes of a plant.
//!
//! Stage C, and the only stage that needs a model. **Strictly advisory**: it writes a
//! sentence into `slot_metrics.diagnosis` and nothing else. No rule reads that column,
//! which is asserted by a test rather than left to discipline — a model that invents a
//! nutrient deficiency must not be able to dose the tank.
//!
//! Three decisions shape this more than the code does.
//!
//! **Not on frame upload.** A 7B vision model on CPU takes 30 to 120 seconds. Putting
//! that in the agent's upload request would time it out, and the agent would retry a
//! frame it had already delivered.
//!
//! **Not on the five-minute dispatcher tick either.** Inference would block the
//! notification sweep behind it. This has its own slow loop.
//!
//! **Only for plants something else already suspects.** Running a model over sixteen
//! healthy plants a day is waste, and it is also the wrong framing: this is a second
//! opinion on a plant the deterministic rules have flagged, not a general survey.

use crate::app::AppState;
use garden_core::{Garden, SlotMetrics};
use garden_vision::diagnose::{DiagnosisBackend, DiagnosisRequest, OllamaConfig};
use garden_vision::roi::RoiMap;
use std::sync::Arc;
use std::time::Duration;

/// How often the loop wakes. Daily: a plant's appearance does not change hourly, and
/// each pass may be minutes of inference.
pub const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Startup delay, so a restart does not immediately queue inference.
const FIRST_RUN_DELAY: Duration = Duration::from_secs(300);

/// Ceiling on slots examined per garden per pass.
///
/// If a dozen plants are flagged at once the useful signal is "this garden is in
/// trouble", which the rules already said. Grinding through all twelve would take half
/// an hour and tell you the same thing.
const MAX_PER_GARDEN: usize = 4;

/// Build the backend from the environment, or nothing.
///
/// Runtime-gated on `GARDEN_OLLAMA_URL` rather than a Cargo feature, matching how ntfy
/// and SMTP work: an operator turns this on by pointing it at a container, not by
/// rebuilding.
pub fn from_env() -> Option<Arc<dyn DiagnosisBackend>> {
    let config = config_from(
        std::env::var("GARDEN_OLLAMA_URL").ok(),
        std::env::var("GARDEN_OLLAMA_MODEL").ok(),
    )?;

    match garden_vision::diagnose::OllamaBackend::new(config.clone()) {
        Ok(backend) => {
            tracing::info!(%config, "visual diagnosis enabled");
            Some(Arc::new(backend))
        }
        Err(error) => {
            tracing::error!(%error, "visual diagnosis backend could not be built");
            None
        }
    }
}

/// The configuration those variables describe, if they describe one.
///
/// Split out from [`from_env`] so it can be tested: reading the process environment is
/// the one thing a test cannot do twice safely, and this crate forbids the `unsafe`
/// that setting it would need.
pub fn config_from(url: Option<String>, model: Option<String>) -> Option<OllamaConfig> {
    let base_url = url.map(|u| u.trim().to_string()).filter(|u| !u.is_empty())?;
    Some(OllamaConfig {
        base_url,
        model: model
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| OllamaConfig::default().model),
        ..OllamaConfig::default()
    })
}

pub fn spawn(state: AppState, backend: Arc<dyn DiagnosisBackend>) {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_RUN_DELAY).await;
        loop {
            if let Err(error) = sweep(&state, backend.as_ref()).await {
                tracing::error!(%error, "diagnosis sweep failed");
            }
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

/// Whether a plant is worth a model's time.
///
/// Pure, so the selection can be tested without inference. The rules have already done
/// the hard part; this only decides whether to ask for a second opinion on what they
/// found.
pub fn worth_asking(metrics: &SlotMetrics) -> bool {
    // A stalled canopy or a yellowing one is a symptom without a cause — exactly where
    // a description of what the leaves look like adds something the numbers cannot.
    metrics.is_stalled() || metrics.is_chlorotic()
}

async fn sweep(
    state: &AppState,
    backend: &dyn DiagnosisBackend,
) -> Result<usize, garden_store::StoreError> {
    let mut asked = 0;
    for garden in state.store.all_gardens().await? {
        match diagnose_garden(state, backend, &garden).await {
            Ok(n) => asked += n,
            // One garden's camera being unreachable must not stop the others.
            Err(error) => tracing::warn!(garden = %garden.id, %error, "skipping garden"),
        }
    }
    if asked > 0 {
        tracing::info!(asked, "visual diagnosis pass complete");
    }
    Ok(asked)
}

async fn diagnose_garden(
    state: &AppState,
    backend: &dyn DiagnosisBackend,
    garden: &Garden,
) -> Result<usize, garden_store::StoreError> {
    let flagged: Vec<SlotMetrics> = state
        .store
        .latest_slot_metrics(garden.id)
        .await?
        .into_iter()
        .filter(worth_asking)
        .take(MAX_PER_GARDEN)
        .collect();
    if flagged.is_empty() {
        return Ok(0);
    }

    // Only a pinned frame will do. An ambient one was taken at whatever brightness the
    // room happened to be, and asking a model whether the leaves look pale would be
    // asking it about the lighting.
    let Some(frame) = state.store.latest_comparable_frame(garden.id).await? else {
        tracing::debug!(
            garden = %garden.id,
            "slots are flagged but there is no comparable frame to show a model"
        );
        return Ok(0);
    };
    let Some(raw) = state.store.roi_map(garden.id).await? else {
        return Ok(0);
    };
    let Ok(map) = serde_json::from_str::<RoiMap>(&raw) else {
        return Ok(0);
    };
    let bytes = state.store.frame_bytes(&frame).await?;
    let Ok(image) = image::load_from_memory(&bytes) else {
        return Ok(0);
    };
    let image = image.to_rgb8();

    let plantings = state.store.active_plantings(garden.id).await?;
    let book = garden_core::VarietyBook::catalogue();
    let now = state.now();

    let mut asked = 0;
    for metrics in flagged {
        let Some(request) = build_request(&image, &map, &metrics, &plantings, &book, now) else {
            continue;
        };
        match backend.describe(request).await {
            Ok(text) if !text.trim().is_empty() => {
                let text = text.trim();
                state.store.set_diagnosis(garden.id, metrics.slot, text).await?;
                asked += 1;
            }
            Ok(_) => tracing::debug!(slot = %metrics.slot, "the model had nothing to say"),
            // A slow or absent model costs a description, not a sweep.
            Err(error) => tracing::warn!(slot = %metrics.slot, %error, "diagnosis failed"),
        }
    }
    Ok(asked)
}

/// Crop the slot out of the frame and describe what is already known about it.
///
/// The measurements go in the prompt so the model is not asked to estimate what has
/// already been measured — its job is to describe what the leaves look like, which is
/// the one thing the pipeline cannot do.
fn build_request(
    image: &image::RgbImage,
    map: &RoiMap,
    metrics: &SlotMetrics,
    plantings: &[garden_core::Planting],
    book: &garden_core::VarietyBook,
    now: garden_core::Timestamp,
) -> Option<DiagnosisRequest> {
    let roi = map.get(metrics.slot)?;
    if !roi.fits(image.width(), image.height()) {
        return None;
    }
    let crop = image::imageops::crop_imm(image, roi.x, roi.y, roi.width, roi.height).to_image();

    let mut png = std::io::Cursor::new(Vec::new());
    crop.write_to(&mut png, image::ImageFormat::Png).ok()?;

    let planting = plantings.iter().find(|p| p.slot == metrics.slot);
    let variety = planting
        .and_then(|p| book.get(&p.variety))
        .map(|v| v.name.clone())
        .unwrap_or_else(|| "an unidentified plant".into());
    let age_days = planting
        .and_then(|p| p.days_since_germination(now))
        .unwrap_or(0.0);

    let mut measured = format!(
        "canopy {:.0} cm², {:.0}% of the leaf area yellowing",
        metrics.canopy_area_cm2,
        metrics.yellowing_index * 100.0
    );
    if metrics.is_stalled() {
        measured.push_str(", and the canopy has stopped expanding");
    }

    Some(DiagnosisRequest {
        image: png.into_inner(),
        variety,
        age_days,
        measured,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use garden_core::{SlotId, Timestamp};

    fn metrics(slot: u8) -> SlotMetrics {
        SlotMetrics::new(
            SlotId(slot),
            Timestamp::from_second(1_700_000_000).unwrap(),
            300.0,
        )
    }

    #[test]
    fn a_healthy_plant_is_not_worth_a_models_time() {
        // Running inference over sixteen fine plants a day is waste, and it is the
        // wrong framing: this is a second opinion, not a survey.
        let mut healthy = metrics(0);
        healthy.growth_rate_cm2_per_day = Some(8.0);
        healthy.yellowing_index = 0.05;
        assert!(!worth_asking(&healthy));
    }

    #[test]
    fn a_stalled_or_yellowing_plant_is() {
        let mut stalled = metrics(0);
        stalled.growth_rate_cm2_per_day = Some(-3.0);
        assert!(worth_asking(&stalled));

        let mut yellow = metrics(1);
        yellow.growth_rate_cm2_per_day = Some(8.0);
        yellow.yellowing_index = 0.6;
        assert!(worth_asking(&yellow));
    }

    #[test]
    fn a_slot_with_no_growth_history_is_not_worth_asking_about() {
        // A fresh measurement has no fitted growth rate — not because the plant
        // stopped, but because nothing has been measured twice yet. Reading that as a
        // symptom would send every newly-watched slot to the model.
        let fresh = metrics(0);
        assert_eq!(fresh.growth_rate_cm2_per_day, None);
        assert!(!worth_asking(&fresh));
    }

    #[test]
    fn the_backend_is_off_unless_pointed_at_one() {
        // Same shape as ntfy and SMTP: configuration turns it on, not a rebuild.
        assert!(config_from(None, None).is_none());
        assert!(config_from(Some("   ".into()), None).is_none(), "blank is not set");
        assert!(config_from(Some(String::new()), None).is_none());
    }

    #[test]
    fn a_configured_url_gives_a_default_model() {
        let config = config_from(Some("http://garden-ollama:11434".into()), None).unwrap();
        assert_eq!(config.base_url, "http://garden-ollama:11434");
        assert_eq!(config.model, OllamaConfig::default().model);
    }

    #[test]
    fn the_model_can_be_overridden_and_whitespace_does_not_count() {
        let config = config_from(
            Some(" http://host:11434 ".into()),
            Some("  llava:13b  ".into()),
        )
        .unwrap();
        assert_eq!(config.base_url, "http://host:11434");
        assert_eq!(config.model, "llava:13b");

        let blank_model = config_from(Some("http://host:11434".into()), Some("  ".into())).unwrap();
        assert_eq!(blank_model.model, OllamaConfig::default().model);
    }
}
