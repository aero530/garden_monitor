//! Deciding what is a leaf, and what colour of leaf it is.
//!
//! Everything here works in HSV rather than RGB, because the useful question is "what
//! hue is this, regardless of how brightly lit it is". Under a light bar that ramps
//! from sunrise to full output, an RGB threshold reclassifies the same leaf several
//! times a day; a hue threshold does not.
//!
//! Two corrections happen before any threshold is applied, and both matter more than
//! the thresholds themselves:
//!
//! 1. **Grey-world white balance.** Grow LEDs are not neutral. If the light bar leans
//!    magenta, every leaf's measured hue shifts toward blue and the green mask quietly
//!    loses its edges. Normalising each channel by the frame's own mean removes the
//!    tint without needing to know what the lamp is.
//! 2. **Shadow rejection.** Deep shade inside a canopy is dark, and dark pixels have
//!    unstable hue — a handful of noisy least-significant bits decides whether a
//!    near-black pixel reads as green or purple. Those pixels are excluded rather than
//!    classified.

/// Hue in degrees, saturation and value in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hsv {
    pub h: f32,
    pub s: f32,
    pub v: f32,
}

pub fn rgb_to_hsv(r: f32, g: f32, b: f32) -> Hsv {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let h = if delta < f32::EPSILON {
        0.0
    } else if max == r {
        60.0 * (((g - b) / delta) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };

    Hsv {
        h: if h < 0.0 { h + 360.0 } else { h },
        s: if max <= f32::EPSILON { 0.0 } else { delta / max },
        v: max,
    }
}

/// Per-channel multipliers that make the frame's average pixel neutral grey.
///
/// Clamped, because a frame that is genuinely mostly one colour — a tray of red
/// lettuce, say — would otherwise have that colour "corrected" away.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WhiteBalance {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl WhiteBalance {
    pub const NEUTRAL: WhiteBalance = WhiteBalance {
        r: 1.0,
        g: 1.0,
        b: 1.0,
    };

    /// How far a single channel may be pushed. Beyond this the frame is not tinted,
    /// it is genuinely that colour.
    const LIMIT: f32 = 1.6;

    /// Estimate from channel means over the whole frame.
    pub fn grey_world(mean_r: f32, mean_g: f32, mean_b: f32) -> Self {
        let grey = (mean_r + mean_g + mean_b) / 3.0;
        if grey <= f32::EPSILON {
            return Self::NEUTRAL;
        }
        let gain = |mean: f32| {
            if mean <= f32::EPSILON {
                1.0
            } else {
                (grey / mean).clamp(1.0 / Self::LIMIT, Self::LIMIT)
            }
        };
        Self {
            r: gain(mean_r),
            g: gain(mean_g),
            b: gain(mean_b),
        }
    }

    pub fn apply(&self, r: f32, g: f32, b: f32) -> (f32, f32, f32) {
        (
            (r * self.r).min(1.0),
            (g * self.g).min(1.0),
            (b * self.b).min(1.0),
        )
    }

    pub fn is_neutral(&self) -> bool {
        (self.r - 1.0).abs() < 1e-3 && (self.g - 1.0).abs() < 1e-3 && (self.b - 1.0).abs() < 1e-3
    }
}

/// What a pixel was judged to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pixel {
    /// Healthy foliage: green through blue-green.
    Foliage,
    /// Foliage that has lost chlorophyll — yellow-green through yellow. Counted as
    /// canopy, and separately as the numerator of the chlorosis index.
    Chlorotic,
    /// Not a plant: yPod, tank, wall, light bar.
    Background,
    /// Too dark to classify. Excluded from every ratio rather than guessed at.
    TooDark,
}

impl Pixel {
    /// Whether this pixel counts toward canopy area.
    pub fn is_canopy(self) -> bool {
        matches!(self, Pixel::Foliage | Pixel::Chlorotic)
    }
}

/// Thresholds for classification.
///
/// Exposed as a struct rather than constants because the right values depend on the
/// light bar's spectrum, and a Studio 2 under a replacement lamp is a different
/// problem from one under the factory LEDs. The defaults are tuned for full-spectrum
/// white.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    /// Hue range counted as healthy foliage, in degrees.
    pub foliage_hue: (f32, f32),
    /// Hue range counted as chlorotic foliage.
    pub chlorotic_hue: (f32, f32),
    /// Below this saturation a pixel is grey structure, not a leaf.
    pub min_saturation: f32,
    /// Below this value a pixel is shadow, and its hue is noise.
    pub min_value: f32,
    /// Above this value a pixel is a blown highlight — a specular reflection off a wet
    /// leaf or the lamp itself — and its hue is equally meaningless.
    pub max_value: f32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            // Yellow-green through cyan. Deliberately wide: purple-leaved varieties
            // are handled by their own hue band rather than by loosening this one.
            foliage_hue: (75.0, 175.0),
            // Yellow through yellow-green. Overlaps the bottom of the foliage band,
            // and is tested first, so the overlap resolves toward chlorotic.
            chlorotic_hue: (35.0, 80.0),
            min_saturation: 0.22,
            min_value: 0.14,
            max_value: 0.97,
        }
    }
}

use serde::{Deserialize, Serialize};

fn in_band(h: f32, band: (f32, f32)) -> bool {
    h >= band.0 && h <= band.1
}

pub fn classify(hsv: Hsv, thresholds: &Thresholds) -> Pixel {
    if hsv.v < thresholds.min_value || hsv.v > thresholds.max_value {
        return Pixel::TooDark;
    }
    if hsv.s < thresholds.min_saturation {
        return Pixel::Background;
    }
    // Chlorotic first: the bands overlap in the yellow-green, and a leaf that is
    // yellowing should be reported as yellowing rather than absorbed into "healthy".
    if in_band(hsv.h, thresholds.chlorotic_hue) {
        return Pixel::Chlorotic;
    }
    if in_band(hsv.h, thresholds.foliage_hue) {
        return Pixel::Foliage;
    }
    Pixel::Background
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hsv(r: u8, g: u8, b: u8) -> Hsv {
        rgb_to_hsv(
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
        )
    }

    #[test]
    fn primaries_convert_to_the_expected_hues() {
        assert!((hsv(255, 0, 0).h - 0.0).abs() < 0.5);
        assert!((hsv(0, 255, 0).h - 120.0).abs() < 0.5);
        assert!((hsv(0, 0, 255).h - 240.0).abs() < 0.5);
        let white = hsv(255, 255, 255);
        assert!(white.s < 1e-6 && (white.v - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_healthy_leaf_reads_as_foliage() {
        let t = Thresholds::default();
        for (r, g, b) in [(60, 140, 40), (34, 120, 55), (80, 160, 90)] {
            assert_eq!(classify(hsv(r, g, b), &t), Pixel::Foliage, "{r},{g},{b}");
        }
    }

    #[test]
    fn a_yellowing_leaf_reads_as_chlorotic_not_healthy() {
        let t = Thresholds::default();
        for (r, g, b) in [(200, 200, 40), (180, 190, 70), (210, 180, 30)] {
            assert_eq!(classify(hsv(r, g, b), &t), Pixel::Chlorotic, "{r},{g},{b}");
        }
    }

    #[test]
    fn chlorotic_pixels_still_count_as_canopy() {
        // A yellowing plant has not shrunk. Dropping these from the area would make
        // "sick" look identical to "harvested".
        assert!(Pixel::Chlorotic.is_canopy());
        assert!(Pixel::Foliage.is_canopy());
        assert!(!Pixel::Background.is_canopy());
        assert!(!Pixel::TooDark.is_canopy());
    }

    #[test]
    fn structure_and_shadow_are_not_leaves() {
        let t = Thresholds::default();
        assert_eq!(classify(hsv(200, 200, 205), &t), Pixel::Background, "grey yPod");
        assert_eq!(classify(hsv(10, 12, 9), &t), Pixel::TooDark, "canopy shade");
        assert_eq!(classify(hsv(254, 254, 254), &t), Pixel::TooDark, "blown highlight");
        assert_eq!(classify(hsv(150, 40, 160), &t), Pixel::Background, "magenta lamp");
    }

    #[test]
    fn grey_world_removes_a_lamp_tint() {
        // A magenta-leaning lamp: red and blue means above green.
        let wb = WhiteBalance::grey_world(0.55, 0.40, 0.52);
        assert!(wb.g > 1.0, "green should be lifted, got {}", wb.g);
        assert!(wb.r < 1.0 && wb.b < 1.0);

        // ...and after correction, the frame's average is neutral again.
        let (r, g, b) = wb.apply(0.55, 0.40, 0.52);
        let spread = r.max(g).max(b) - r.min(g).min(b);
        assert!(spread < 0.05, "still tinted: {r} {g} {b}");
    }

    #[test]
    fn white_balance_cannot_correct_away_a_genuinely_red_frame() {
        // A tray of red lettuce is not a tinted lamp. Without the clamp, grey-world
        // would "fix" the plants into looking green.
        let wb = WhiteBalance::grey_world(0.80, 0.10, 0.10);
        assert!(wb.g <= WhiteBalance::LIMIT + 1e-6);
        assert!(wb.r >= 1.0 / WhiteBalance::LIMIT - 1e-6);
    }

    #[test]
    fn a_neutral_frame_produces_no_correction() {
        let wb = WhiteBalance::grey_world(0.5, 0.5, 0.5);
        assert!(wb.is_neutral());
        assert_eq!(wb.apply(0.3, 0.4, 0.5), (0.3, 0.4, 0.5));
    }

    #[test]
    fn white_balance_makes_a_tinted_leaf_classifiable_again() {
        // The point of the whole exercise. Under a magenta-heavy lamp this leaf's raw
        // hue falls outside the foliage band; after correction it does not.
        let t = Thresholds::default();
        let (r, g, b) = (0.28, 0.30, 0.24);
        assert_ne!(classify(rgb_to_hsv(r, g, b), &t), Pixel::Foliage);

        let wb = WhiteBalance::grey_world(0.55, 0.40, 0.52);
        let (r, g, b) = wb.apply(r, g, b);
        assert_eq!(classify(rgb_to_hsv(r, g, b), &t), Pixel::Foliage);
    }
}
