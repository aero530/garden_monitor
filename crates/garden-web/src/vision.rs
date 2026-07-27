//! Running the vision pipeline over an uploaded frame.
//!
//! Analysis happens on upload rather than on the dispatcher tick, because the bytes
//! are already in memory here and re-reading them from disk five minutes later to do
//! the same work would be strictly worse. It costs the agent tens of milliseconds on a
//! request it makes once an hour.
//!
//! **A garden with no ROI map is not analysed, and that is the whole switch.** Nothing
//! else has to be configured, and there is no state where vision is "enabled" but
//! cannot say where slot 7 is.

use garden_core::{GardenId, SlotId, Timestamp};
use garden_store::Store;
use garden_vision::{Analyzer, growth, roi::RoiMap};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Analyse a frame and store what it found.
///
/// Returns how many slots were measured, or `None` when the garden has no calibration.
/// Every failure below that is logged and swallowed: a frame that cannot be analysed
/// must still be *stored*, because the photograph is useful to a person even when it
/// is useless to the pipeline.
pub async fn analyse_and_store(
    store: &Store,
    garden: GardenId,
    frame_id: Uuid,
    bytes: &[u8],
    at: Timestamp,
) -> Option<usize> {
    let raw = match store.roi_map(garden).await {
        Ok(Some(raw)) => raw,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(%error, %garden, "could not load the ROI map");
            return None;
        }
    };

    let map: RoiMap = match serde_json::from_str(&raw) {
        Ok(map) => map,
        Err(error) => {
            tracing::error!(%error, %garden, "stored ROI map is not valid; vision is off");
            return None;
        }
    };

    let analyzer = Analyzer::new(map);
    let mut report = match analyzer.analyse(bytes, at) {
        Ok(report) => report,
        Err(error) => {
            tracing::warn!(%error, %garden, "frame could not be analysed");
            return None;
        }
    };

    for (slot, reason) in &report.skipped {
        tracing::debug!(%garden, %slot, %reason, "slot not measured");
    }

    // Growth needs history, which lives in the database rather than in the frame.
    let history = load_history(store, garden, &report.slots, at).await;
    garden_vision::apply_growth(&mut report, &history, at);

    if let Err(error) = store
        .record_slot_metrics(garden, frame_id, &report.slots)
        .await
    {
        tracing::error!(%error, %garden, "could not store slot metrics");
        return None;
    }
    if let Some(algae) = report.algae
        && let Err(error) = store.record_algae(garden, frame_id, algae).await
    {
        tracing::error!(%error, %garden, "could not store the algae reading");
    }

    Some(report.slots.len())
}

async fn load_history(
    store: &Store,
    garden: GardenId,
    slots: &[garden_core::SlotMetrics],
    now: Timestamp,
) -> BTreeMap<SlotId, Vec<growth::Sample>> {
    let since = garden_core::time::add_days(now, -growth::WINDOW_DAYS);
    let mut history = BTreeMap::new();
    for metrics in slots {
        match store.canopy_history(garden, metrics.slot, since).await {
            Ok(samples) => {
                history.insert(
                    metrics.slot,
                    samples
                        .into_iter()
                        .map(|(at, area_cm2)| growth::Sample { at, area_cm2 })
                        .collect(),
                );
            }
            // One slot's missing history costs that slot its growth rate, not the
            // whole frame its analysis.
            Err(error) => tracing::warn!(%error, slot = %metrics.slot, "no canopy history"),
        }
    }
    history
}

#[cfg(test)]
mod tests {
    use super::*;
    use garden_auth::EmailAddress;
    use garden_core::{DeviceModel, Geometry};
    use garden_store::frames::{FrameSource, NewFrame};
    use image::{Rgb, RgbImage};

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    async fn fixture() -> (Store, GardenId) {
        let store = Store::in_memory().await.unwrap();
        let user = store
            .create_user(
                EmailAddress::parse("phil@example.com").unwrap(),
                "Phil",
                "a long enough password",
                t0(),
            )
            .await
            .unwrap();
        let garden = store
            .create_garden("Kitchen", DeviceModel::Studio2, "UTC", user.id, t0())
            .await
            .unwrap();
        (store, garden.id)
    }

    fn calibrated_map() -> RoiMap {
        let mut map = RoiMap::grid(&Geometry::STUDIO_2, 320, 480, 0.1);
        for slot in &mut map.slots {
            slot.cm2_per_px = 0.05;
        }
        map.scale_measured = true;
        map
    }

    /// A PNG of the tower with leaf-coloured blocks in the named slots.
    fn frame_bytes(map: &RoiMap, planted: &[u8], fill: f32) -> Vec<u8> {
        let mut image = RgbImage::from_pixel(320, 480, Rgb([120, 118, 122]));
        for roi in &map.slots {
            if !planted.contains(&roi.slot.0) {
                continue;
            }
            let rows = (roi.height as f32 * fill) as u32;
            for y in roi.y..(roi.y + rows) {
                for x in roi.x..(roi.x + roi.width) {
                    image.put_pixel(x, y, Rgb([60, 140, 40]));
                }
            }
        }
        let mut out = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode");
        out.into_inner()
    }

    async fn store_frame(store: &Store, garden: GardenId, bytes: &[u8], at: Timestamp) -> Uuid {
        store
            .put_frame(NewFrame {
                garden,
                captured_at: at,
                width: 320,
                height: 480,
                light_duty_milli: Some(800),
                comparable: true,
                source: FrameSource::Agent,
                bytes,
            })
            .await
            .unwrap()
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn a_garden_without_calibration_is_not_analysed() {
        let (store, garden) = fixture().await;
        let bytes = frame_bytes(&calibrated_map(), &[0], 0.5);
        let frame = store_frame(&store, garden, &bytes, t0()).await;

        assert_eq!(
            analyse_and_store(&store, garden, frame, &bytes, t0()).await,
            None
        );
        assert_eq!(store.slot_metrics_count(garden).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_calibrated_garden_measures_every_slot() {
        let (store, garden) = fixture().await;
        let map = calibrated_map();
        store
            .save_roi_map(garden, &serde_json::to_string(&map).unwrap(), t0())
            .await
            .unwrap();

        let bytes = frame_bytes(&map, &[0, 9], 0.5);
        let frame = store_frame(&store, garden, &bytes, t0()).await;
        assert_eq!(
            analyse_and_store(&store, garden, frame, &bytes, t0()).await,
            Some(16)
        );

        let latest = store.latest_slot_metrics(garden).await.unwrap();
        let planted: Vec<u8> = latest
            .iter()
            .filter(|m| m.canopy_area_cm2 > 0.0)
            .map(|m| m.slot.0)
            .collect();
        assert_eq!(planted, vec![0, 9]);
    }

    #[tokio::test]
    async fn growth_rate_appears_once_there_is_history() {
        let (store, garden) = fixture().await;
        let map = calibrated_map();
        store
            .save_roi_map(garden, &serde_json::to_string(&map).unwrap(), t0())
            .await
            .unwrap();

        // Four frames over six days, the plant filling more of its slot each time.
        for (day, fill) in [(0.0, 0.2), (2.0, 0.35), (4.0, 0.5), (6.0, 0.65)] {
            let at = garden_core::time::add_days(t0(), day);
            let bytes = frame_bytes(&map, &[3], fill);
            let frame = store_frame(&store, garden, &bytes, at).await;
            analyse_and_store(&store, garden, frame, &bytes, at).await;
        }

        let latest = store.latest_slot_metrics(garden).await.unwrap();
        let slot = latest.iter().find(|m| m.slot == SlotId(3)).unwrap();
        assert!(
            slot.growth_rate_cm2_per_day.is_some_and(|rate| rate > 1.0),
            "expected measured growth, got {slot:?}"
        );
        assert!(!slot.is_stalled());
    }

    #[tokio::test]
    async fn a_corrupt_roi_map_disables_vision_without_losing_the_frame() {
        // The photograph is useful to a person even when it is useless to the
        // pipeline, so a bad map must not turn an upload into an error.
        let (store, garden) = fixture().await;
        store.save_roi_map(garden, "{not json", t0()).await.unwrap();

        let bytes = frame_bytes(&calibrated_map(), &[0], 0.5);
        let frame = store_frame(&store, garden, &bytes, t0()).await;
        assert_eq!(
            analyse_and_store(&store, garden, frame, &bytes, t0()).await,
            None
        );
        assert!(store.find_frame(garden, frame).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_frame_of_the_wrong_size_is_stored_but_not_measured() {
        let (store, garden) = fixture().await;
        let map = calibrated_map();
        store
            .save_roi_map(garden, &serde_json::to_string(&map).unwrap(), t0())
            .await
            .unwrap();

        let mut out = std::io::Cursor::new(Vec::new());
        RgbImage::from_pixel(640, 480, Rgb([120, 118, 122]))
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        let bytes = out.into_inner();
        let frame = store_frame(&store, garden, &bytes, t0()).await;

        assert_eq!(
            analyse_and_store(&store, garden, frame, &bytes, t0()).await,
            None
        );
        assert!(store.find_frame(garden, frame).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_night_frame_records_nothing_rather_than_sixteen_dead_plants() {
        let (store, garden) = fixture().await;
        let map = calibrated_map();
        store
            .save_roi_map(garden, &serde_json::to_string(&map).unwrap(), t0())
            .await
            .unwrap();

        let mut out = std::io::Cursor::new(Vec::new());
        RgbImage::from_pixel(320, 480, Rgb([5, 5, 6]))
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        let bytes = out.into_inner();
        let frame = store_frame(&store, garden, &bytes, t0()).await;

        assert_eq!(
            analyse_and_store(&store, garden, frame, &bytes, t0()).await,
            Some(0)
        );
        assert_eq!(store.slot_metrics_count(garden).await.unwrap(), 0);
    }
}
