//! Turning measurements of the real device into the constants the rules use.
//!
//! Two things need calibrating, and both are currently placeholders in the source:
//! the tank's distance-to-volume mapping, and where each slot sits in the camera
//! frame. Neither can be guessed, and both are cheap to measure once.

use garden_core::{Geometry, TankGeometry};
use garden_vision::roi::{Roi, RoiMap, SlotRoi};
use image::{Rgb, RgbImage};
use std::path::Path;

// --- Tank ---------------------------------------------------------------------------

/// One (distance, volume) pair from the calibration jug.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TankSample {
    pub distance_mm: f32,
    pub volume_l: f32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CalibrationError {
    #[error("need at least two measurements at different levels")]
    NotEnoughSamples,
    #[error("every measurement reported the same distance — is the sensor reading at all?")]
    NoRange,
    #[error("distance increases with volume; the sensor measures down to the water, so it must fall as the tank fills")]
    Inverted,
}

/// Fit `TankGeometry` from a set of measurements.
///
/// Two points would do the arithmetic, but two points also record both of your
/// measurement errors permanently. An ultrasonic sensor reading a rippling surface is
/// noisy by nature, so this fits a line through as many as you took and extrapolates
/// the endpoints from it.
pub fn fit_tank(samples: &[TankSample], capacity_l: f32) -> Result<TankGeometry, CalibrationError> {
    if samples.len() < 2 {
        return Err(CalibrationError::NotEnoughSamples);
    }

    let n = samples.len() as f64;
    let mean_v = samples.iter().map(|s| f64::from(s.volume_l)).sum::<f64>() / n;
    let mean_d = samples.iter().map(|s| f64::from(s.distance_mm)).sum::<f64>() / n;

    let mut covariance = 0.0;
    let mut variance = 0.0;
    for s in samples {
        let dv = f64::from(s.volume_l) - mean_v;
        covariance += dv * (f64::from(s.distance_mm) - mean_d);
        variance += dv * dv;
    }
    if variance < 1e-6 {
        return Err(CalibrationError::NotEnoughSamples);
    }

    // distance = intercept + slope * volume. Slope must be negative: the sensor looks
    // down at the water, so a fuller tank is a nearer surface.
    let slope = covariance / variance;
    if slope.abs() < 1e-6 {
        return Err(CalibrationError::NoRange);
    }
    if slope > 0.0 {
        return Err(CalibrationError::Inverted);
    }
    let intercept = mean_d - slope * mean_v;

    Ok(TankGeometry {
        capacity_l,
        full_distance_mm: (intercept + slope * f64::from(capacity_l)) as f32,
        empty_distance_mm: intercept as f32,
    })
}

/// Largest disagreement between the fit and the measurements, in litres.
///
/// Reported so a bad measurement is visible rather than averaged in. On a 15 L tank a
/// residual over about half a litre usually means one reading was taken while the
/// surface was still moving.
pub fn worst_residual_l(geometry: &TankGeometry, samples: &[TankSample]) -> f32 {
    samples
        .iter()
        .map(|s| (geometry.volume_from_distance(s.distance_mm) - s.volume_l).abs())
        .fold(0.0, f32::max)
}

// --- Vision -------------------------------------------------------------------------

/// Scale a grid map from a known real-world width.
///
/// You measure one thing with a ruler — the width of a yPod, say — and count how many
/// pixels it covers in the frame. Everything else follows, per slot, because slots
/// further down the tower are further from the camera and their pixels cover more
/// ground.
pub fn set_scale(map: &mut RoiMap, reference_cm: f32, reference_px: f32) {
    if reference_px <= 0.0 || reference_cm <= 0.0 {
        return;
    }
    let cm_per_px = reference_cm / reference_px;
    let cm2 = cm_per_px * cm_per_px;
    for slot in &mut map.slots {
        slot.cm2_per_px = cm2;
    }
    map.scale_measured = true;
}

/// Draw the ROI rectangles onto a frame, so they can be checked by eye.
///
/// There is no GUI here and there should not be one — this runs over SSH on a server.
/// An annotated PNG is the honest substitute: adjust numbers, re-render, look. Slots
/// are outlined in green, the tank region in amber.
pub fn overlay(image: &RgbImage, map: &RoiMap) -> RgbImage {
    let mut out = image.clone();
    for slot in &map.slots {
        draw_rect(
            &mut out,
            slot.x,
            slot.y,
            slot.width,
            slot.height,
            Rgb([40, 220, 90]),
        );
        // A tick in the corner, so a rectangle that has drifted onto its neighbour is
        // obvious rather than merely plausible.
        draw_marks(&mut out, slot, Rgb([40, 220, 90]));
    }
    if let Some(tank) = &map.tank {
        draw_rect(
            &mut out,
            tank.x,
            tank.y,
            tank.width,
            tank.height,
            Rgb([245, 170, 40]),
        );
    }
    out
}

fn draw_rect(image: &mut RgbImage, x: u32, y: u32, w: u32, h: u32, colour: Rgb<u8>) {
    if w == 0 || h == 0 {
        return;
    }
    let x1 = (x + w - 1).min(image.width().saturating_sub(1));
    let y1 = (y + h - 1).min(image.height().saturating_sub(1));
    for px in x..=x1 {
        if px < image.width() {
            if y < image.height() {
                image.put_pixel(px, y, colour);
            }
            if y1 < image.height() {
                image.put_pixel(px, y1, colour);
            }
        }
    }
    for py in y..=y1 {
        if py < image.height() {
            if x < image.width() {
                image.put_pixel(x, py, colour);
            }
            if x1 < image.width() {
                image.put_pixel(x1, py, colour);
            }
        }
    }
}

/// A short bar in the top-left of a slot, `slot.0 + 1` pixels tall, so the rectangle
/// can be identified without reading numbers off the image.
fn draw_marks(image: &mut RgbImage, slot: &SlotRoi, colour: Rgb<u8>) {
    let length = u32::from(slot.slot.0) + 1;
    for i in 0..length.min(slot.height) {
        let (x, y) = (slot.x + 2, slot.y + 2 + i);
        if x < image.width() && y < image.height() {
            image.put_pixel(x, y, colour);
        }
    }
}

/// A starting map for a garden, with a tank region across the bottom of the frame.
pub fn starting_map(geometry: &Geometry, width: u32, height: u32, margin: f32) -> RoiMap {
    let mut map = RoiMap::grid(geometry, width, height, margin);
    // The reservoir sits under the tower, so the bottom strip is the usual place to
    // look for algae. Certain to need moving; being present is what prompts that.
    map.tank = Some(Roi {
        x: 0,
        y: height.saturating_sub(height / 10),
        width,
        height: height / 10,
    });
    map
}

pub fn write_png(image: &RgbImage, path: &Path) -> std::io::Result<()> {
    image
        .save_with_format(path, image::ImageFormat::Png)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(pairs: &[(f32, f32)]) -> Vec<TankSample> {
        pairs
            .iter()
            .map(|(distance_mm, volume_l)| TankSample {
                distance_mm: *distance_mm,
                volume_l: *volume_l,
            })
            .collect()
    }

    #[test]
    fn a_clean_two_point_measurement_recovers_the_geometry() {
        // Empty reads 330 mm, 15 L reads 60 mm.
        let fitted = fit_tank(&samples(&[(330.0, 0.0), (60.0, 15.0)]), 15.0).unwrap();
        assert!((fitted.empty_distance_mm - 330.0).abs() < 0.1);
        assert!((fitted.full_distance_mm - 60.0).abs() < 0.1);
        assert!(worst_residual_l(&fitted, &samples(&[(330.0, 0.0), (60.0, 15.0)])) < 0.01);
    }

    #[test]
    fn extra_samples_average_out_measurement_noise() {
        // Same underlying line, each reading off by a few millimetres.
        let noisy = samples(&[
            (328.0, 0.0),
            (283.0, 2.5),
            (241.0, 5.0),
            (192.0, 7.5),
            (152.0, 10.0),
            (104.0, 12.5),
            (62.0, 15.0),
        ]);
        let fitted = fit_tank(&noisy, 15.0).unwrap();
        assert!((fitted.empty_distance_mm - 330.0).abs() < 4.0, "{fitted:?}");
        assert!((fitted.full_distance_mm - 60.0).abs() < 4.0, "{fitted:?}");
        assert!(worst_residual_l(&fitted, &noisy) < 0.4);
    }

    #[test]
    fn a_bad_reading_shows_up_in_the_residual_rather_than_hiding() {
        let mut bad = samples(&[(330.0, 0.0), (240.0, 5.0), (150.0, 10.0), (60.0, 15.0)]);
        let clean = fit_tank(&bad, 15.0).unwrap();
        assert!(worst_residual_l(&clean, &bad) < 0.1);

        // One measurement taken while the surface was still sloshing.
        bad.push(TankSample {
            distance_mm: 200.0,
            volume_l: 2.0,
        });
        let fitted = fit_tank(&bad, 15.0).unwrap();
        assert!(
            worst_residual_l(&fitted, &bad) > 0.5,
            "a bad sample should be visible"
        );
    }

    #[test]
    fn one_measurement_is_not_a_calibration() {
        assert_eq!(
            fit_tank(&samples(&[(300.0, 1.0)]), 15.0),
            Err(CalibrationError::NotEnoughSamples)
        );
        assert_eq!(fit_tank(&[], 15.0), Err(CalibrationError::NotEnoughSamples));
    }

    #[test]
    fn measurements_at_one_level_are_not_a_calibration_either() {
        assert_eq!(
            fit_tank(&samples(&[(300.0, 5.0), (301.0, 5.0), (299.0, 5.0)]), 15.0),
            Err(CalibrationError::NotEnoughSamples)
        );
    }

    #[test]
    fn a_sensor_wired_backwards_is_caught_rather_than_fitted() {
        // Distance rising with volume means the reading is inverted somewhere. Fitting
        // it anyway produces a tank that reports full when it is empty.
        assert_eq!(
            fit_tank(&samples(&[(60.0, 0.0), (330.0, 15.0)]), 15.0),
            Err(CalibrationError::Inverted)
        );
    }

    #[test]
    fn a_dead_sensor_reading_a_constant_is_caught() {
        let flat = samples(&[(200.0, 0.0), (200.0, 7.5), (200.0, 15.0)]);
        assert_eq!(fit_tank(&flat, 15.0), Err(CalibrationError::NoRange));
    }

    #[test]
    fn setting_the_scale_converts_a_ruler_measurement_into_area() {
        let mut map = RoiMap::grid(&Geometry::STUDIO_2, 1920, 1080, 0.1);
        assert!(!map.is_calibrated());

        // A 7 cm yPod covering 70 pixels: 0.1 cm per pixel, so 0.01 cm² each — which
        // happens to be exactly the placeholder, and must still count as measured.
        set_scale(&mut map, 7.0, 70.0);
        assert!((map.slots[0].cm2_per_px - 0.01).abs() < 1e-6);
        assert!(map.is_calibrated());

        set_scale(&mut map, 7.0, 35.0);
        assert!((map.slots[0].cm2_per_px - 0.04).abs() < 1e-6);
        assert!(map.is_calibrated());
    }

    #[test]
    fn a_nonsense_scale_is_ignored_rather_than_producing_infinity() {
        let mut map = RoiMap::grid(&Geometry::STUDIO_2, 1920, 1080, 0.1);
        let before = map.slots[0].cm2_per_px;
        set_scale(&mut map, 7.0, 0.0);
        set_scale(&mut map, 0.0, 70.0);
        set_scale(&mut map, -3.0, 70.0);
        assert_eq!(map.slots[0].cm2_per_px, before);
    }

    #[test]
    fn the_overlay_draws_inside_the_frame_and_marks_every_slot() {
        let map = starting_map(&Geometry::STUDIO_2, 320, 480, 0.1);
        let image = RgbImage::from_pixel(320, 480, Rgb([10, 10, 10]));
        let drawn = overlay(&image, &map);

        assert_eq!(drawn.dimensions(), (320, 480));
        let green = drawn
            .pixels()
            .filter(|p| p.0 == [40, 220, 90])
            .count();
        let amber = drawn.pixels().filter(|p| p.0 == [245, 170, 40]).count();
        assert!(green > 16 * 4, "every slot should be outlined");
        assert!(amber > 0, "the tank region should be outlined too");
    }

    #[test]
    fn the_overlay_survives_a_rectangle_at_the_very_edge() {
        let mut map = RoiMap::grid(&Geometry::STUDIO_2, 64, 64, 0.0);
        map.slots[0] = SlotRoi {
            slot: garden_core::SlotId(0),
            x: 60,
            y: 60,
            width: 4,
            height: 4,
            cm2_per_px: 0.01,
        };
        let image = RgbImage::from_pixel(64, 64, Rgb([0, 0, 0]));
        // Must not panic on the boundary.
        let _ = overlay(&image, &map);
    }

    #[test]
    fn a_starting_map_includes_somewhere_to_look_for_algae() {
        let map = starting_map(&Geometry::STUDIO_2, 1920, 1080, 0.1);
        let tank = map.tank.expect("a tank region");
        assert_eq!(tank.width, 1920);
        assert!(tank.y + tank.height <= 1080);
        assert!(map.validate(1920, 1080).is_ok());
    }
}
