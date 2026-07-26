//! Algae and biofilm coverage on the tank.
//!
//! Same masking machinery as the canopy, pointed at a region where green means the
//! opposite of healthy. A tank lid should be the colour of plastic; anything green on
//! it is growth that competes for nutrients and clogs the pump.
//!
//! The distinction from canopy measurement is entirely in *where you look*, which is
//! why this is thirty lines and not its own pipeline.

use crate::color::{Pixel, Thresholds, WhiteBalance, classify, rgb_to_hsv};
use crate::roi::Roi;
use gardyn_core::AlgaeReading;
use image::RgbImage;
use jiff::Timestamp;

/// Fraction of the tank region showing growth.
///
/// Shadow is excluded from the denominator for the same reason as in the canopy pass:
/// a dark photograph of a clean tank must not read as a clean tank *or* a dirty one,
/// it must read as very little having been seen. Here that surfaces as a coverage
/// computed over the lit portion only.
pub fn measure(
    image: &RgbImage,
    roi: &Roi,
    white_balance: &WhiteBalance,
    thresholds: &Thresholds,
    at: Timestamp,
) -> Option<AlgaeReading> {
    if roi.width == 0 || roi.height == 0 {
        return None;
    }
    if roi.x + roi.width > image.width() || roi.y + roi.height > image.height() {
        return None;
    }

    let (mut growth, mut classifiable) = (0u32, 0u32);
    for y in roi.y..(roi.y + roi.height) {
        for x in roi.x..(roi.x + roi.width) {
            let p = image.get_pixel(x, y);
            let (r, g, b) = white_balance.apply(
                f32::from(p[0]) / 255.0,
                f32::from(p[1]) / 255.0,
                f32::from(p[2]) / 255.0,
            );
            match classify(rgb_to_hsv(r, g, b), thresholds) {
                Pixel::TooDark => {}
                // Both bands count. Biofilm goes yellow-brown as it thickens, and a
                // tank that has got that far is further gone than a green one.
                Pixel::Foliage | Pixel::Chlorotic => {
                    growth += 1;
                    classifiable += 1;
                }
                Pixel::Background => classifiable += 1,
            }
        }
    }

    // Too little of the region was visible to say anything. Returning a coverage of
    // 0.0 here would be a confident "the tank is clean" derived from a dark frame.
    let total = roi.width * roi.height;
    if classifiable * 100 < total * MIN_VISIBLE_PERCENT {
        return None;
    }

    Some(AlgaeReading {
        at,
        coverage: growth as f32 / classifiable.max(1) as f32,
    })
}

/// How much of the tank region has to be lit before a reading is worth having.
const MIN_VISIBLE_PERCENT: u32 = 40;

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn roi() -> Roi {
        Roi {
            x: 10,
            y: 10,
            width: 100,
            height: 50,
        }
    }

    fn tank(clean_colour: [u8; 3], green_rows: u32) -> RgbImage {
        let mut image = RgbImage::from_pixel(200, 200, Rgb(clean_colour));
        for y in 10..(10 + green_rows) {
            for x in 10..110 {
                image.put_pixel(x, y, Rgb([70, 150, 60]));
            }
        }
        image
    }

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    #[test]
    fn a_clean_tank_reads_as_clean() {
        let reading = measure(
            &tank([190, 190, 195], 0),
            &roi(),
            &WhiteBalance::NEUTRAL,
            &Thresholds::default(),
            t0(),
        )
        .unwrap();
        assert_eq!(reading.coverage, 0.0);
        assert!(!reading.is_advisory());
    }

    #[test]
    fn coverage_tracks_how_much_of_the_lid_is_green() {
        // 10 of 50 rows.
        let reading = measure(
            &tank([190, 190, 195], 10),
            &roi(),
            &WhiteBalance::NEUTRAL,
            &Thresholds::default(),
            t0(),
        )
        .unwrap();
        assert!((reading.coverage - 0.2).abs() < 0.01, "{reading:?}");
        assert!(reading.is_advisory() && !reading.is_urgent());
    }

    #[test]
    fn a_badly_fouled_tank_is_urgent() {
        let reading = measure(
            &tank([190, 190, 195], 25),
            &roi(),
            &WhiteBalance::NEUTRAL,
            &Thresholds::default(),
            t0(),
        )
        .unwrap();
        assert!(reading.is_urgent(), "{reading:?}");
    }

    #[test]
    fn brown_biofilm_counts_as_growth_too() {
        let mut image = RgbImage::from_pixel(200, 200, Rgb([190, 190, 195]));
        for y in 10..30 {
            for x in 10..110 {
                image.put_pixel(x, y, Rgb([170, 165, 50]));
            }
        }
        let reading = measure(
            &image,
            &roi(),
            &WhiteBalance::NEUTRAL,
            &Thresholds::default(),
            t0(),
        )
        .unwrap();
        assert!(reading.coverage > 0.3, "{reading:?}");
    }

    #[test]
    fn a_dark_frame_gives_no_reading_rather_than_a_clean_one() {
        // The failure that matters: "coverage 0.0" from a night-time capture would
        // stand down the conditioner rule on a tank nobody has actually looked at.
        let dark = RgbImage::from_pixel(200, 200, Rgb([6, 6, 7]));
        assert_eq!(
            measure(
                &dark,
                &roi(),
                &WhiteBalance::NEUTRAL,
                &Thresholds::default(),
                t0()
            ),
            None
        );
    }

    #[test]
    fn a_region_outside_the_frame_is_refused() {
        let image = RgbImage::from_pixel(50, 50, Rgb([190, 190, 195]));
        assert_eq!(
            measure(
                &image,
                &roi(),
                &WhiteBalance::NEUTRAL,
                &Thresholds::default(),
                t0()
            ),
            None
        );
    }
}
