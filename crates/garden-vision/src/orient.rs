//! Turning a frame the right way up.
//!
//! The Studio 2's camera sits sideways in the light bar: frames arrive with the towers
//! running horizontally and the tank at image-right. Everything downstream assumes
//! otherwise — `roi` maps pixel rectangles onto slots numbered top to bottom, `growth`
//! compares canopy areas across months of frames, and a person looking at the dashboard
//! expects a tower to be vertical.
//!
//! **Applied once, at ingest.** The stored frame is upright, so no consumer has to
//! remember to rotate and none can disagree with another about the coordinate space. The
//! alternative — store as shot, rotate on read — puts the same obligation in every
//! consumer and gets it wrong in whichever one is added last.
//!
//! Only right angles. A quarter turn permutes pixels exactly; an arbitrary angle
//! interpolates them, and interpolating before measuring canopy area would invent green
//! that was never photographed.

use garden_core::CameraRotation;

#[derive(Debug, thiserror::Error)]
pub enum OrientError {
    #[error("not a decodable image: {0}")]
    Decode(String),
    #[error("could not re-encode the rotated frame: {0}")]
    Encode(String),
}

/// A frame after rotation, with the dimensions it now has rather than the ones it had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Oriented {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// JPEG quality for the re-encode.
///
/// High, because this is the only lossy step we add to a frame and everything measured
/// afterwards is measured from it. A quarter turn could in principle be done losslessly
/// by transposing the JPEG's own blocks, but the `image` crate does not offer that, and
/// 92 is visually and numerically indistinguishable at the scale canopy metrics work at.
const JPEG_QUALITY: u8 = 92;

/// Longest edge a stored frame may have.
///
/// The agent captures the camera's full sensor, because on this hardware the smaller
/// modes are centre crops rather than downscales and asking for one silently costs
/// field of view. That makes an 8 MP frame, which is far more than canopy area needs
/// and would be an hourly 2–3 MB on disk forever.
///
/// Resizing here is close to free: this function already decodes and re-encodes every
/// rotated frame, so the scale happens inside a pass that was happening anyway — and
/// on the brain rather than on a single ARMv6 core that would take a minute over it.
///
/// 1440 leaves a Studio 2 slot around 540×180 px, which is an order of magnitude more
/// than a canopy-area threshold resolves.
pub const MAX_STORED_EDGE: u32 = 1440;

/// Rotate a frame upright and bring it down to a storable size.
///
/// Returning the input untouched matters when there is genuinely nothing to do — a
/// Home garden, or a simulated one, whose frame is already small enough never pays a
/// decode-and-re-encode for a no-op, and its frames stay bit-identical to what the
/// camera produced.
///
/// `max_edge` of `None` disables the resize, for a caller that wants the rotation
/// alone.
pub fn orient(
    bytes: &[u8],
    rotation: CameraRotation,
    max_edge: Option<u32>,
) -> Result<Oriented, OrientError> {
    let decoded = image::load_from_memory(bytes).map_err(|e| OrientError::Decode(e.to_string()))?;
    let oversized = max_edge.is_some_and(|max| decoded.width().max(decoded.height()) > max);

    if rotation.is_none() && !oversized {
        return Ok(Oriented {
            bytes: bytes.to_vec(),
            width: decoded.width(),
            height: decoded.height(),
        });
    }

    let turned = match rotation {
        CameraRotation::None => decoded,
        CameraRotation::Clockwise90 => decoded.rotate90(),
        CameraRotation::Clockwise180 => decoded.rotate180(),
        CameraRotation::Clockwise270 => decoded.rotate270(),
    };

    // Triangle rather than Lanczos3. Lanczos is sharper on a photograph a person
    // looks at, and it overshoots at edges — a bright halo outside every leaf, which
    // a green-fraction threshold counts as leaf. Averaging neighbours cannot invent a
    // pixel greener than the ones it came from.
    let turned = match max_edge {
        Some(max) if turned.width().max(turned.height()) > max => {
            let scale = f64::from(max) / f64::from(turned.width().max(turned.height()));
            let width = ((f64::from(turned.width()) * scale).round() as u32).max(1);
            let height = ((f64::from(turned.height()) * scale).round() as u32).max(1);
            turned.resize_exact(width, height, image::imageops::FilterType::Triangle)
        }
        _ => turned,
    };

    // Straight to RGB8: the frames are JPEG from a webcam, so there is no alpha to
    // preserve, and the JPEG encoder refuses an image that carries one.
    let rgb = turned.to_rgb8();
    let mut out = Vec::with_capacity(bytes.len());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .encode_image(&image::DynamicImage::ImageRgb8(rgb))
        .map_err(|e| OrientError::Encode(e.to_string()))?;

    Ok(Oriented {
        bytes: out,
        width: turned.width(),
        height: turned.height(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    /// A frame with a distinguishable corner, so a rotation is detectable rather than
    /// merely plausible. Red is at top-left; everything else is black.
    fn marked(width: u32, height: u32) -> Vec<u8> {
        let mut img = RgbImage::from_pixel(width, height, Rgb([0, 0, 0]));
        for y in 0..height / 4 {
            for x in 0..width / 4 {
                img.put_pixel(x, y, Rgb([255, 0, 0]));
            }
        }
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 95)
            .encode_image(&image::DynamicImage::ImageRgb8(img))
            .unwrap();
        out
    }

    fn redness_at(bytes: &[u8], fx: f32, fy: f32) -> u8 {
        let img = image::load_from_memory(bytes).unwrap().to_rgb8();
        let x = ((img.width() - 1) as f32 * fx) as u32;
        let y = ((img.height() - 1) as f32 * fy) as u32;
        img.get_pixel(x, y).0[0]
    }

    #[test]
    fn a_quarter_turn_swaps_the_dimensions() {
        // The Studio 2's actual case: 1920×1080 landscape becomes 1080×1920 portrait,
        // which is what the ROI map has to be sized against.
        let oriented = orient(&marked(320, 180), CameraRotation::Clockwise90, None).unwrap();
        assert_eq!((oriented.width, oriented.height), (180, 320));
    }

    #[test]
    fn a_clockwise_quarter_turn_sends_the_top_left_corner_to_the_top_right() {
        // The direction is the whole point, and getting it backwards is a 180° error
        // that no dimension check would catch.
        let oriented = orient(&marked(320, 180), CameraRotation::Clockwise90, None).unwrap();
        assert!(redness_at(&oriented.bytes, 0.9, 0.1) > 200, "should be red");
        assert!(redness_at(&oriented.bytes, 0.1, 0.1) < 60, "should be black");
    }

    #[test]
    fn no_rotation_returns_the_original_bytes_untouched() {
        // Not merely equivalent: identical. A Home garden should not pay a lossy
        // re-encode for a rotation of zero.
        let original = marked(64, 48);
        let oriented = orient(&original, CameraRotation::None, None).unwrap();
        assert_eq!(oriented.bytes, original);
        assert_eq!((oriented.width, oriented.height), (64, 48));
    }

    #[test]
    fn a_half_turn_keeps_the_dimensions_and_moves_the_corner_diagonally() {
        let oriented = orient(&marked(320, 180), CameraRotation::Clockwise180, None).unwrap();
        assert_eq!((oriented.width, oriented.height), (320, 180));
        assert!(redness_at(&oriented.bytes, 0.9, 0.9) > 200);
        assert!(redness_at(&oriented.bytes, 0.1, 0.1) < 60);
    }

    #[test]
    fn three_quarters_is_the_other_way_round_from_one() {
        let cw = orient(&marked(320, 180), CameraRotation::Clockwise90, None).unwrap();
        let ccw = orient(&marked(320, 180), CameraRotation::Clockwise270, None).unwrap();
        assert_eq!((cw.width, cw.height), (ccw.width, ccw.height));
        // Clockwise puts the marked corner top-right; anticlockwise, bottom-left.
        assert!(redness_at(&cw.bytes, 0.9, 0.1) > 200);
        assert!(redness_at(&ccw.bytes, 0.1, 0.9) > 200);
    }

    #[test]
    fn an_oversized_frame_comes_down_to_the_stored_limit_keeping_its_shape() {
        // The Studio 2's real case: a 3264×2448 sensor, turned a quarter and stored.
        let oriented = orient(
            &marked(3264, 2448),
            CameraRotation::Clockwise90,
            Some(MAX_STORED_EDGE),
        )
        .unwrap();

        assert_eq!(oriented.width.max(oriented.height), MAX_STORED_EDGE);
        assert_eq!((oriented.width, oriented.height), (1080, 1440));
        // 3:4 after the turn, because the sensor is 4:3. A frame that came back 9:16
        // would mean the agent had been given a cropped mode again.
        let ratio = f64::from(oriented.width) / f64::from(oriented.height);
        assert!((ratio - 3.0 / 4.0).abs() < 0.01, "{ratio:.3}");
    }

    #[test]
    fn a_frame_already_small_enough_is_not_resized() {
        let oriented = orient(&marked(320, 180), CameraRotation::None, Some(MAX_STORED_EDGE))
            .unwrap();
        assert_eq!((oriented.width, oriented.height), (320, 180));
    }

    #[test]
    fn an_unrotated_frame_is_still_brought_down_to_size() {
        // The `CameraRotation::None` fast path used to return the input untouched
        // whatever its size, which was right when this only rotated. A Home garden
        // sending 8 MP would have stored 8 MP forever.
        let oriented = orient(&marked(2000, 1000), CameraRotation::None, Some(1000)).unwrap();
        assert_eq!((oriented.width, oriented.height), (1000, 500));
    }

    #[test]
    fn downscaling_does_not_invent_colour_the_camera_never_saw() {
        // Why Triangle and not Lanczos3. Lanczos overshoots at a hard edge, ringing a
        // brighter-than-source halo around every leaf — which a green-fraction
        // threshold then counts as leaf. Averaging cannot exceed its inputs.
        let mut image = RgbImage::from_pixel(800, 800, Rgb([0, 0, 0]));
        for y in 0..800 {
            for x in 0..400 {
                image.put_pixel(x, y, Rgb([0, 200, 0]));
            }
        }
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100)
            .encode_image(&image::DynamicImage::ImageRgb8(image))
            .unwrap();

        let oriented = orient(&bytes, CameraRotation::None, Some(200)).unwrap();
        let out = image::load_from_memory(&oriented.bytes).unwrap().to_rgb8();
        // A little JPEG slack, but nowhere near the overshoot a ringing filter makes.
        let brightest = out.pixels().map(|p| p.0[1]).max().unwrap();
        assert!(brightest <= 215, "green reached {brightest}, above the source 200");
    }

    #[test]
    fn something_that_is_not_an_image_is_an_error_rather_than_a_panic() {
        // The upload path sniffs content types, but a truncated body from a Pi on
        // household wifi is a normal event.
        assert!(orient(b"not an image at all", CameraRotation::Clockwise90, None).is_err());
        assert!(orient(&[], CameraRotation::None, None).is_err());
    }
}
