//! Turning a camera frame into per-slot measurements.
//!
//! One ultra-wide camera on the light bar sees the whole tower. This crate undistorts
//! what it produces, cuts it into per-slot regions, and measures each one.
//!
//! Three stages, independently switchable, exactly as [`gardyn_core::Capability`]
//! models them:
//!
//! | Stage | Capability | Cost | Produces |
//! |---|---|---|---|
//! | A | `CanopyMetrics` | negligible | area, green fraction, chlorosis, growth rate |
//! | B | `PlantSegmentation` | small | seedling count, flowering |
//! | C | `VisualDiagnosis` | heavy | a sentence of plain language |
//!
//! Stage A is roughly 80% of the value and is the default. B is a connected-component
//! count over the mask A already produced, so it is nearly free — the ONNX model the
//! design anticipated turns out to be optional, and the trait is there for when a
//! model earns its weight. C is the only one that needs a GPU-sized dependency, and it
//! is strictly advisory: it returns a `String` and there is no path from it to a task.
//!
//! ```no_run
//! use gardyn_core::Timestamp;
//! use gardyn_vision::{Analyzer, roi::RoiMap};
//!
//! let map: RoiMap = serde_json::from_str(&std::fs::read_to_string("rois.json")?)?;
//! let analyzer = Analyzer::new(map);
//! let report = analyzer.analyse(&std::fs::read("frame.jpg")?, Timestamp::now())?;
//!
//! for metrics in &report.slots {
//!     println!("{} — {:.0} cm²", metrics.slot, metrics.canopy_area_cm2);
//! }
//! for (slot, why) in &report.skipped {
//!     eprintln!("{slot} not measured: {why}");
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod algae;
pub mod canopy;
pub mod color;
pub mod diagnose;
pub mod growth;
pub mod lens;
pub mod roi;
pub mod segment;

use canopy::CanopyReading;
use color::{Pixel, Thresholds, WhiteBalance, classify, rgb_to_hsv};
use gardyn_core::{AlgaeReading, SlotId, SlotMetrics, Timestamp};
use image::RgbImage;
use roi::{RoiError, RoiMap};
use segment::{ConnectedComponents, Segmenter};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum VisionError {
    #[error("could not decode the frame: {0}")]
    Decode(String),
    #[error(transparent)]
    Roi(#[from] RoiError),
}

/// Everything one frame produced.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameReport {
    pub at: Timestamp,
    pub slots: Vec<SlotMetrics>,
    pub algae: Option<AlgaeReading>,
    /// Slots whose reading was discarded, and why.
    ///
    /// Surfaced rather than silently dropped: sixteen slots reporting and one missing
    /// is a fact worth seeing, and "the frame was too dark" is a different problem
    /// from "the plant died".
    pub skipped: Vec<(SlotId, SkipReason)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Most of the rectangle was shadow or blown highlight.
    Unreadable,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::Unreadable => f.write_str("too dark or too bright to classify"),
        }
    }
}

/// The pipeline.
pub struct Analyzer {
    map: RoiMap,
    thresholds: Thresholds,
    segmenter: Box<dyn Segmenter>,
    /// Off unless a map has real scale. Without it the areas are in arbitrary units,
    /// and a harvest threshold in cm² would be comparing against nonsense.
    absolute_scale: bool,
}

impl Analyzer {
    pub fn new(map: RoiMap) -> Self {
        let absolute_scale = map.is_calibrated();
        Self {
            map,
            thresholds: Thresholds::default(),
            segmenter: Box::new(ConnectedComponents::default()),
            absolute_scale,
        }
    }

    pub fn with_thresholds(mut self, thresholds: Thresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    pub fn with_segmenter(mut self, segmenter: Box<dyn Segmenter>) -> Self {
        self.segmenter = segmenter;
        self
    }

    pub fn map(&self) -> &RoiMap {
        &self.map
    }

    /// Whether areas from this analyzer are in real cm² or arbitrary units.
    pub fn has_absolute_scale(&self) -> bool {
        self.absolute_scale
    }

    /// Decode and measure a frame.
    ///
    /// Stage A and B always run — B is a flood fill over a mask A already built, so
    /// making it optional would save nothing worth the branch. Growth rate is left at
    /// zero here because it needs history; [`apply_growth`] fills it in.
    pub fn analyse(&self, bytes: &[u8], at: Timestamp) -> Result<FrameReport, VisionError> {
        let image = image::load_from_memory(bytes)
            .map_err(|e| VisionError::Decode(e.to_string()))?
            .to_rgb8();
        self.analyse_image(&image, at)
    }

    pub fn analyse_image(
        &self,
        image: &RgbImage,
        at: Timestamp,
    ) -> Result<FrameReport, VisionError> {
        self.map.validate(image.width(), image.height())?;
        let white_balance = canopy::estimate_white_balance(image);

        let mut slots = Vec::new();
        let mut skipped = Vec::new();

        for roi in &self.map.slots {
            let (reading, mask) = canopy::measure(
                image,
                roi,
                &self.map.lens,
                &white_balance,
                &self.thresholds,
            );

            if !reading.is_trustworthy() {
                skipped.push((roi.slot, SkipReason::Unreadable));
                continue;
            }

            let petals = count_petals(image, roi, &white_balance, &self.thresholds);
            let segmentation = self.segmenter.segment(&mask, petals);

            slots.push(to_metrics(roi.slot, at, &reading, segmentation));
        }

        let algae = self.map.tank.as_ref().and_then(|tank| {
            algae::measure(image, tank, &white_balance, &self.thresholds, at)
        });

        Ok(FrameReport {
            at,
            slots,
            algae,
            skipped,
        })
    }
}

fn to_metrics(
    slot: SlotId,
    at: Timestamp,
    reading: &CanopyReading,
    segmentation: segment::Segmentation,
) -> SlotMetrics {
    let mut metrics = SlotMetrics::new(slot, at, reading.area_cm2);
    metrics.green_fraction = reading.green_fraction;
    metrics.yellowing_index = reading.yellowing_index;
    metrics.plant_count = segmentation.plant_count;
    metrics.flowering = segmentation.flowering;
    metrics
}

/// Pixels that are lit and saturated but in neither foliage band — petals.
fn count_petals(
    image: &RgbImage,
    roi: &roi::SlotRoi,
    white_balance: &WhiteBalance,
    thresholds: &Thresholds,
) -> u32 {
    let mut petals = 0;
    for y in roi.y..(roi.y + roi.height) {
        for x in roi.x..(roi.x + roi.width) {
            let p = image.get_pixel(x, y);
            let (r, g, b) = white_balance.apply(
                f32::from(p[0]) / 255.0,
                f32::from(p[1]) / 255.0,
                f32::from(p[2]) / 255.0,
            );
            let hsv = rgb_to_hsv(r, g, b);
            // Background covers both structure and petals; the saturation test is what
            // separates a yellow flower from a beige yPod.
            if classify(hsv, thresholds) == Pixel::Background && hsv.s >= PETAL_SATURATION {
                petals += 1;
            }
        }
    }
    petals
}

/// Petals are vivid. Structure is not.
const PETAL_SATURATION: f32 = 0.45;

/// Fill in growth rates from stored history.
///
/// Separate from [`Analyzer::analyse`] because a frame does not know what came before
/// it. The caller holds the history — in this system, the database — and this keeps
/// the pipeline itself a pure function of one image.
pub fn apply_growth(
    report: &mut FrameReport,
    history: &BTreeMap<SlotId, Vec<growth::Sample>>,
    now: Timestamp,
) {
    for metrics in &mut report.slots {
        let Some(samples) = history.get(&metrics.slot) else {
            continue;
        };
        let mut recent = growth::window(samples, now, growth::WINDOW_DAYS);
        // Include the reading just taken, which is not in the store yet.
        recent.push(growth::Sample {
            at: metrics.at,
            area_cm2: metrics.canopy_area_cm2,
        });
        if let Some(rate) = growth::fit_rate(&recent) {
            metrics.growth_rate_cm2_per_day = rate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gardyn_core::{Geometry, LensCalibration};
    use image::Rgb;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn map() -> RoiMap {
        let mut map = RoiMap::grid(&Geometry::STUDIO_2, 320, 480, 0.1);
        for slot in &mut map.slots {
            slot.cm2_per_px = 0.05;
        }
        map.scale_measured = true;
        map
    }

    /// A frame with grey structure and a leaf-coloured block in the named slots.
    fn tower(map: &RoiMap, planted: &[u8], fill: f32) -> RgbImage {
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
        image
    }

    #[test]
    fn an_empty_tower_measures_zero_everywhere() {
        let map = map();
        let report = Analyzer::new(map.clone())
            .analyse_image(&tower(&map, &[], 0.0), t0())
            .unwrap();
        assert_eq!(report.slots.len(), 16);
        assert!(report.slots.iter().all(|m| m.canopy_area_cm2 == 0.0));
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn only_the_planted_slots_measure_canopy() {
        let map = map();
        let report = Analyzer::new(map.clone())
            .analyse_image(&tower(&map, &[0, 5, 11], 0.5), t0())
            .unwrap();

        let grown: Vec<u8> = report
            .slots
            .iter()
            .filter(|m| m.canopy_area_cm2 > 0.0)
            .map(|m| m.slot.0)
            .collect();
        assert_eq!(grown, vec![0, 5, 11]);
    }

    #[test]
    fn a_fuller_slot_measures_a_larger_canopy() {
        let map = map();
        let small = Analyzer::new(map.clone())
            .analyse_image(&tower(&map, &[3], 0.25), t0())
            .unwrap();
        let large = Analyzer::new(map.clone())
            .analyse_image(&tower(&map, &[3], 0.75), t0())
            .unwrap();

        let area = |r: &FrameReport| {
            r.slots.iter().find(|m| m.slot.0 == 3).unwrap().canopy_area_cm2
        };
        assert!(area(&large) > area(&small) * 2.5);
    }

    #[test]
    fn a_night_frame_skips_every_slot_instead_of_reporting_dead_plants() {
        // The failure this guards: an unlit capture measuring zero canopy everywhere,
        // which downstream is indistinguishable from every plant having died.
        let map = map();
        let dark = RgbImage::from_pixel(320, 480, Rgb([5, 5, 6]));
        let report = Analyzer::new(map).analyse_image(&dark, t0()).unwrap();

        assert!(report.slots.is_empty());
        assert_eq!(report.skipped.len(), 16);
        assert!(report.skipped.iter().all(|(_, r)| *r == SkipReason::Unreadable));
    }

    #[test]
    fn a_frame_of_the_wrong_size_is_an_error_not_a_guess() {
        let map = map();
        let wrong = RgbImage::from_pixel(640, 480, Rgb([120, 118, 122]));
        assert!(matches!(
            Analyzer::new(map).analyse_image(&wrong, t0()),
            Err(VisionError::Roi(RoiError::WrongFrameSize { .. }))
        ));
    }

    #[test]
    fn an_uncalibrated_map_says_so_rather_than_reporting_fake_centimetres() {
        let uncalibrated = RoiMap::grid(&Geometry::STUDIO_2, 320, 480, 0.1);
        assert!(!Analyzer::new(uncalibrated).has_absolute_scale());
        assert!(Analyzer::new(map()).has_absolute_scale());
    }

    #[test]
    fn seedlings_are_counted_through_the_full_pipeline() {
        let map = map();
        let mut image = RgbImage::from_pixel(320, 480, Rgb([120, 118, 122]));
        let roi = map.get(SlotId(2)).unwrap();
        // Three separated sprouts inside one slot's rectangle.
        for i in 0..3u32 {
            let x0 = roi.x + 2 + i * (roi.width / 3);
            for y in roi.y..(roi.y + 10) {
                for x in x0..(x0 + 7).min(roi.x + roi.width) {
                    image.put_pixel(x, y, Rgb([60, 140, 40]));
                }
            }
        }

        let report = Analyzer::new(map).analyse_image(&image, t0()).unwrap();
        let slot = report.slots.iter().find(|m| m.slot == SlotId(2)).unwrap();
        assert_eq!(slot.plant_count, Some(3));
    }

    #[test]
    fn algae_is_only_measured_when_a_tank_region_is_configured() {
        let map = map();
        let image = tower(&map, &[], 0.0);
        assert!(Analyzer::new(map.clone()).analyse_image(&image, t0()).unwrap().algae.is_none());

        let mut with_tank = map;
        with_tank.tank = Some(roi::Roi {
            x: 0,
            y: 440,
            width: 320,
            height: 40,
        });
        let report = Analyzer::new(with_tank).analyse_image(&image, t0()).unwrap();
        assert_eq!(report.algae.map(|a| a.coverage), Some(0.0));
    }

    #[test]
    fn growth_rate_needs_history_and_is_zero_until_it_has_some() {
        let map = map();
        let mut report = Analyzer::new(map.clone())
            .analyse_image(&tower(&map, &[0], 0.5), t0())
            .unwrap();
        assert_eq!(report.slots[0].growth_rate_cm2_per_day, 0.0);

        let mut history = BTreeMap::new();
        history.insert(
            SlotId(0),
            vec![
                growth::Sample {
                    at: gardyn_core::time::add_days(t0(), -6.0),
                    area_cm2: 40.0,
                },
                growth::Sample {
                    at: gardyn_core::time::add_days(t0(), -4.0),
                    area_cm2: 60.0,
                },
                growth::Sample {
                    at: gardyn_core::time::add_days(t0(), -2.0),
                    area_cm2: 80.0,
                },
            ],
        );
        apply_growth(&mut report, &history, t0());
        assert!(report.slots[0].growth_rate_cm2_per_day > 5.0);
    }

    #[test]
    fn lens_distortion_reaches_the_reported_area() {
        // End-to-end version of the lens test: the same plant in the same rectangle
        // measures larger once a real calibration is attached to the map.
        let flat = map();
        let mut curved = flat.clone();
        curved.lens = LensCalibration {
            fx: 300.0,
            fy: 300.0,
            cx: 160.0,
            cy: 240.0,
            distortion: [-0.30, 0.10, 0.0, 0.0, 0.0],
        };

        let image = tower(&flat, &[7], 0.6);
        let corner_area = |m: RoiMap| {
            Analyzer::new(m)
                .analyse_image(&image, t0())
                .unwrap()
                .slots
                .iter()
                .find(|s| s.slot == SlotId(7))
                .unwrap()
                .canopy_area_cm2
        };
        assert!(corner_area(curved) > corner_area(flat) * 1.05);
    }
}
