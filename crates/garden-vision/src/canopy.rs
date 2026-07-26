//! Stage A: canopy area, green fraction and chlorosis from one region of interest.
//!
//! No machine learning, negligible CPU, and roughly 80% of what vision is worth. It is
//! the default stage for that reason.

use crate::color::{Pixel, Thresholds, WhiteBalance, classify, rgb_to_hsv};
use crate::lens;
use crate::roi::SlotRoi;
use garden_core::LensCalibration;
use image::RgbImage;

/// What one ROI measured, before it is combined with history.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanopyReading {
    /// Corrected for lens distortion and converted to real units.
    pub area_cm2: f32,
    /// Canopy pixels as a fraction of the *classifiable* pixels in the ROI.
    pub green_fraction: f32,
    /// Chlorotic pixels as a fraction of canopy pixels.
    pub yellowing_index: f32,
    /// Pixels excluded as shadow or blown highlight, as a fraction of the ROI.
    ///
    /// Reported rather than hidden: a reading where most of the rectangle was too dark
    /// to classify is not a small plant, it is a bad photograph, and the two must not
    /// look the same downstream.
    pub unclassified_fraction: f32,
}

impl CanopyReading {
    /// Above this share of unclassifiable pixels the reading is not trustworthy.
    ///
    /// Usually means the capture happened during the dark hours, or the light bar was
    /// mid-ramp. Either way the answer is to discard it, not to average it in.
    pub const MAX_UNCLASSIFIED: f32 = 0.6;

    pub fn is_trustworthy(&self) -> bool {
        self.unclassified_fraction <= Self::MAX_UNCLASSIFIED
    }
}

/// Estimate the frame's white balance from a subsample of its pixels.
///
/// Every 16th pixel in each direction — 256× fewer samples, and the channel means of a
/// two-megapixel image are not meaningfully different for it.
pub fn estimate_white_balance(image: &RgbImage) -> WhiteBalance {
    const STRIDE: u32 = 16;
    let (mut r, mut g, mut b, mut n) = (0.0f64, 0.0f64, 0.0f64, 0u32);
    for y in (0..image.height()).step_by(STRIDE as usize) {
        for x in (0..image.width()).step_by(STRIDE as usize) {
            let p = image.get_pixel(x, y);
            r += f64::from(p[0]);
            g += f64::from(p[1]);
            b += f64::from(p[2]);
            n += 1;
        }
    }
    if n == 0 {
        return WhiteBalance::NEUTRAL;
    }
    let scale = 255.0 * f64::from(n);
    WhiteBalance::grey_world(
        (r / scale) as f32,
        (g / scale) as f32,
        (b / scale) as f32,
    )
}

/// Classify every pixel in an ROI into a mask, and measure it.
///
/// The mask is returned alongside the reading because [`crate::segment`] needs it, and
/// classifying the same pixels twice to get it would double the cost of the pipeline.
pub fn measure(
    image: &RgbImage,
    roi: &SlotRoi,
    lens_calibration: &LensCalibration,
    white_balance: &WhiteBalance,
    thresholds: &Thresholds,
) -> (CanopyReading, Mask) {
    let mut mask = Mask::new(roi.width, roi.height);
    let (mut canopy, mut chlorotic, mut dark) = (0u32, 0u32, 0u32);

    for row in 0..roi.height {
        for column in 0..roi.width {
            let p = image.get_pixel(roi.x + column, roi.y + row);
            let (r, g, b) = white_balance.apply(
                f32::from(p[0]) / 255.0,
                f32::from(p[1]) / 255.0,
                f32::from(p[2]) / 255.0,
            );
            let pixel = classify(rgb_to_hsv(r, g, b), thresholds);
            match pixel {
                Pixel::Foliage => canopy += 1,
                Pixel::Chlorotic => {
                    canopy += 1;
                    chlorotic += 1;
                }
                Pixel::TooDark => dark += 1,
                Pixel::Background => {}
            }
            mask.set(column, row, pixel.is_canopy());
        }
    }

    let total = roi.pixel_count().max(1) as f32;
    let classifiable = (roi.pixel_count().saturating_sub(dark)).max(1) as f32;

    // The area correction is evaluated once at the ROI centre rather than per pixel.
    // Distortion varies over the whole frame; across one slot's rectangle it is
    // effectively constant, and a per-pixel Jacobian would cost 16× the work of the
    // classification it corrects.
    let (cx, cy) = roi.centre();
    let correction = lens::area_scale_at(lens_calibration, cx, cy);

    let reading = CanopyReading {
        area_cm2: canopy as f32 * roi.cm2_per_px * correction,
        green_fraction: canopy as f32 / classifiable,
        yellowing_index: if canopy == 0 {
            0.0
        } else {
            chlorotic as f32 / canopy as f32
        },
        unclassified_fraction: dark as f32 / total,
    };
    (reading, mask)
}

/// A one-bit-per-pixel canopy mask for one ROI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mask {
    pub width: u32,
    pub height: u32,
    bits: Vec<bool>,
}

impl Mask {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            bits: vec![false; (width as usize) * (height as usize)],
        }
    }

    pub fn set(&mut self, x: u32, y: u32, value: bool) {
        if let Some(slot) = self.index(x, y) {
            self.bits[slot] = value;
        }
    }

    pub fn get(&self, x: u32, y: u32) -> bool {
        self.index(x, y).is_some_and(|i| self.bits[i])
    }

    pub fn count(&self) -> u32 {
        self.bits.iter().filter(|b| **b).count() as u32
    }

    fn index(&self, x: u32, y: u32) -> Option<usize> {
        (x < self.width && y < self.height)
            .then(|| (y as usize) * (self.width as usize) + (x as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use garden_core::SlotId;
    use image::Rgb;

    /// A frame with a solid block of `colour` filling the given rectangle.
    fn frame_with(w: u32, h: u32, rect: (u32, u32, u32, u32), colour: [u8; 3]) -> RgbImage {
        let mut image = RgbImage::from_pixel(w, h, Rgb([90, 90, 95])); // grey structure
        let (rx, ry, rw, rh) = rect;
        for y in ry..(ry + rh) {
            for x in rx..(rx + rw) {
                image.put_pixel(x, y, Rgb(colour));
            }
        }
        image
    }

    fn roi(x: u32, y: u32, w: u32, h: u32) -> SlotRoi {
        SlotRoi {
            slot: SlotId(0),
            x,
            y,
            width: w,
            height: h,
            cm2_per_px: 0.02,
        }
    }

    #[test]
    fn a_solid_leaf_block_measures_its_own_area() {
        let image = frame_with(200, 200, (50, 50, 40, 30), [60, 140, 40]);
        let (reading, mask) = measure(
            &image,
            &roi(50, 50, 40, 30),
            &LensCalibration::IDENTITY,
            &WhiteBalance::NEUTRAL,
            &Thresholds::default(),
        );

        assert_eq!(mask.count(), 40 * 30);
        assert!((reading.area_cm2 - 40.0 * 30.0 * 0.02).abs() < 0.01);
        assert!((reading.green_fraction - 1.0).abs() < 1e-6);
        assert_eq!(reading.yellowing_index, 0.0);
    }

    #[test]
    fn grey_structure_is_not_counted_as_canopy() {
        let image = frame_with(200, 200, (0, 0, 1, 1), [60, 140, 40]);
        let (reading, mask) = measure(
            &image,
            &roi(50, 50, 40, 30),
            &LensCalibration::IDENTITY,
            &WhiteBalance::NEUTRAL,
            &Thresholds::default(),
        );
        assert_eq!(mask.count(), 0);
        assert_eq!(reading.area_cm2, 0.0);
        assert_eq!(reading.green_fraction, 0.0);
    }

    #[test]
    fn half_a_yellow_canopy_reads_as_half_chlorotic() {
        let mut image = frame_with(100, 100, (10, 10, 20, 20), [60, 140, 40]);
        for y in 10..20 {
            for x in 10..30 {
                image.put_pixel(x, y, Rgb([200, 200, 40]));
            }
        }
        let (reading, _) = measure(
            &image,
            &roi(10, 10, 20, 20),
            &LensCalibration::IDENTITY,
            &WhiteBalance::NEUTRAL,
            &Thresholds::default(),
        );
        assert!((reading.yellowing_index - 0.5).abs() < 0.01, "{reading:?}");
        // ...and the plant has not shrunk.
        assert!((reading.green_fraction - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_dark_frame_is_flagged_rather_than_read_as_a_dead_plant() {
        // A capture during the dark hours. Every pixel is unclassifiable, and the
        // difference between "no plant" and "no light" has to survive to the caller.
        let image = RgbImage::from_pixel(100, 100, Rgb([6, 7, 6]));
        let (reading, _) = measure(
            &image,
            &roi(10, 10, 40, 40),
            &LensCalibration::IDENTITY,
            &WhiteBalance::NEUTRAL,
            &Thresholds::default(),
        );
        assert_eq!(reading.area_cm2, 0.0);
        assert!(reading.unclassified_fraction > 0.9);
        assert!(!reading.is_trustworthy());
    }

    #[test]
    fn shadowed_pixels_do_not_dilute_the_green_fraction() {
        // Half the rectangle is deep shade. The plant fills the lit half, so it is
        // 100% of what could be seen, not 50% of the rectangle.
        let mut image = RgbImage::from_pixel(100, 100, Rgb([5, 5, 5]));
        for y in 10..30 {
            for x in 10..20 {
                image.put_pixel(x, y, Rgb([60, 140, 40]));
            }
        }
        let (reading, _) = measure(
            &image,
            &roi(10, 10, 20, 20),
            &LensCalibration::IDENTITY,
            &WhiteBalance::NEUTRAL,
            &Thresholds::default(),
        );
        assert!((reading.green_fraction - 1.0).abs() < 1e-6, "{reading:?}");
        assert!((reading.unclassified_fraction - 0.5).abs() < 0.01);
    }

    #[test]
    fn lens_correction_enlarges_a_plant_at_the_frame_edge() {
        let calibration = LensCalibration {
            fx: 900.0,
            fy: 900.0,
            cx: 960.0,
            cy: 540.0,
            distortion: [-0.32, 0.11, 0.0, 0.0, -0.02],
        };
        let leaf = [60u8, 140, 40];
        let centre_roi = roi(940, 520, 40, 40);
        let edge_roi = roi(1860, 1020, 40, 40);
        let mut image = RgbImage::from_pixel(1920, 1080, Rgb([90, 90, 95]));
        for r in [&centre_roi, &edge_roi] {
            for y in r.y..(r.y + r.height) {
                for x in r.x..(r.x + r.width) {
                    image.put_pixel(x, y, Rgb(leaf));
                }
            }
        }

        let m = |r: &SlotRoi| {
            measure(
                &image,
                r,
                &calibration,
                &WhiteBalance::NEUTRAL,
                &Thresholds::default(),
            )
            .0
            .area_cm2
        };
        // Identical pixel counts, but the edge patch covers more of the world.
        assert!(m(&edge_roi) > m(&centre_roi) * 1.2, "{} vs {}", m(&edge_roi), m(&centre_roi));
    }

    #[test]
    fn white_balance_is_estimated_from_the_whole_frame() {
        let magenta_lit = RgbImage::from_pixel(320, 240, Rgb([140, 100, 133]));
        let wb = estimate_white_balance(&magenta_lit);
        assert!(wb.g > 1.0 && wb.r < 1.0);

        let neutral = RgbImage::from_pixel(320, 240, Rgb([128, 128, 128]));
        assert!(estimate_white_balance(&neutral).is_neutral());
    }
}
