//! Undistorting the ultra-wide lens.
//!
//! Barrel distortion pulls the image in toward the centre, so a plant near the edge of
//! the frame occupies fewer pixels than an identical plant in the middle. Left
//! uncorrected, the outer column reads as systematically smaller and the harvest rule
//! under-fires on it — a bias, not noise, so no amount of averaging removes it.
//!
//! The whole image is never undistorted. Remapping two megapixels to measure sixteen
//! rectangles is wasted work, and resampling invents pixels that then get counted. The
//! area of a region is corrected instead by evaluating the **Jacobian determinant** of
//! the undistortion map at its centre, which is exactly the local factor by which the
//! lens scaled that patch.

use garden_core::LensCalibration;

/// Apply the Brown–Conrady model: ideal normalised coordinates to distorted ones.
///
/// Same coefficient ordering OpenCV uses, so a calibration produced by
/// `cv2.calibrateCamera` can be pasted straight in. This is the direction the
/// coefficients are *defined* in — it says where a real-world point lands on the
/// sensor — which is the opposite of what measuring an image needs.
pub fn distort_normalised(calibration: &LensCalibration, xn: f32, yn: f32) -> (f32, f32) {
    let [k1, k2, p1, p2, k3] = calibration.distortion;
    let r2 = xn * xn + yn * yn;
    let radial = 1.0 + k1 * r2 + k2 * r2 * r2 + k3 * r2 * r2 * r2;

    let dx = 2.0 * p1 * xn * yn + p2 * (r2 + 2.0 * xn * xn);
    let dy = p1 * (r2 + 2.0 * yn * yn) + 2.0 * p2 * xn * yn;

    (xn * radial + dx, yn * radial + dy)
}

/// Undistort a pixel coordinate, returning ideal normalised image-plane coordinates.
///
/// The distortion model has no closed-form inverse, so this is the fixed-point
/// iteration OpenCV's `undistortPoints` uses: guess that the ideal point is the
/// observed one, apply the model, and correct by the error. It converges in a handful
/// of steps for any physically plausible lens.
///
/// Getting the direction right matters more than it sounds. Running the forward model
/// here instead would produce an area correction that *shrinks* edge plants — doubling
/// the very bias the correction exists to remove, while looking entirely reasonable.
pub fn undistort_point(calibration: &LensCalibration, x: f32, y: f32) -> (f32, f32) {
    let observed_x = (x - calibration.cx) / calibration.fx;
    let observed_y = (y - calibration.cy) / calibration.fy;
    if calibration.is_identity() {
        return (observed_x, observed_y);
    }

    let [k1, k2, p1, p2, k3] = calibration.distortion;
    let (mut xn, mut yn) = (observed_x, observed_y);
    for _ in 0..ITERATIONS {
        let r2 = xn * xn + yn * yn;
        let radial = 1.0 + k1 * r2 + k2 * r2 * r2 + k3 * r2 * r2 * r2;
        if radial.abs() < 1e-6 {
            break;
        }
        let dx = 2.0 * p1 * xn * yn + p2 * (r2 + 2.0 * xn * xn);
        let dy = p1 * (r2 + 2.0 * yn * yn) + 2.0 * p2 * xn * yn;
        xn = (observed_x - dx) / radial;
        yn = (observed_y - dy) / radial;
    }
    (xn, yn)
}

/// Enough for a lens this wide; the iteration is contractive and settles well before.
const ITERATIONS: u32 = 8;

/// How much the lens scaled area at a point, as a multiplier to correct by.
///
/// Computed as the Jacobian determinant of [`undistort_point`], by finite difference.
/// A value above 1.0 means the lens compressed that patch and the measured pixel count
/// understates the real area.
///
/// The step is a whole pixel deliberately: the distortion field varies over hundreds of
/// pixels, so a smaller step buys nothing and starts losing precision in `f32`.
pub fn area_scale_at(calibration: &LensCalibration, x: f32, y: f32) -> f32 {
    if calibration.is_identity() {
        return 1.0;
    }
    const STEP: f32 = 1.0;

    // Central differences, not forward. A one-sided difference is biased by half a
    // step, which shows up as the correction differing between mirror-image points
    // that are physically identical — small, but a systematic error in exactly the
    // quantity this function exists to remove.
    let (xl, yl) = undistort_point(calibration, x - STEP, y);
    let (xr, yr) = undistort_point(calibration, x + STEP, y);
    let (xu, yu) = undistort_point(calibration, x, y - STEP);
    let (xd, yd) = undistort_point(calibration, x, y + STEP);

    // Columns of the Jacobian, in normalised units per pixel.
    let (a, c) = ((xr - xl) / (2.0 * STEP), (yr - yl) / (2.0 * STEP));
    let (b, d) = ((xd - xu) / (2.0 * STEP), (yd - yu) / (2.0 * STEP));

    let determinant = (a * d - b * c).abs();
    // At the optical centre the map is the identity scaled by 1/fx, 1/fy, so normalise
    // by that to get a factor of 1.0 in the middle of the frame rather than a number
    // that depends on the focal length.
    let reference = 1.0 / (calibration.fx * calibration.fy);
    if reference.abs() < f32::EPSILON || !determinant.is_finite() {
        return 1.0;
    }

    let scale = determinant / reference;
    // A calibration with wild coefficients can produce a nonsensical factor at the
    // very corner of the frame. Clamping keeps one bad ROI from producing a plant with
    // a canopy the size of a table.
    scale.clamp(MIN_SCALE, MAX_SCALE)
}

/// Bounds on the area correction.
///
/// An ultra-wide lens compresses the corners by roughly 2-3×; anything outside this is
/// a calibration error rather than optics.
const MIN_SCALE: f32 = 0.2;
const MAX_SCALE: f32 = 5.0;

#[cfg(test)]
mod tests {
    use super::*;

    /// Plausible calibration for a 1920×1080 ultra-wide module.
    fn wide() -> LensCalibration {
        LensCalibration {
            fx: 900.0,
            fy: 900.0,
            cx: 960.0,
            cy: 540.0,
            distortion: [-0.32, 0.11, 0.0, 0.0, -0.02],
        }
    }

    #[test]
    fn an_identity_calibration_changes_nothing() {
        let id = LensCalibration::IDENTITY;
        assert_eq!(area_scale_at(&id, 100.0, 100.0), 1.0);
        assert_eq!(area_scale_at(&id, 1900.0, 1000.0), 1.0);
    }

    #[test]
    fn the_optical_centre_is_undistorted_by_definition() {
        let c = wide();
        let (x, y) = undistort_point(&c, c.cx, c.cy);
        assert!(x.abs() < 1e-6 && y.abs() < 1e-6);
        assert!((area_scale_at(&c, c.cx, c.cy) - 1.0).abs() < 0.01);
    }

    #[test]
    fn undistort_inverts_the_distortion_model() {
        // The property that matters: round-tripping a point through both directions
        // has to return it. If this holds, the Jacobian is the right Jacobian.
        let c = wide();
        for (px, py) in [(300.0, 200.0), (1600.0, 900.0), (960.0, 100.0)] {
            let (ux, uy) = undistort_point(&c, px, py);
            let (dx, dy) = distort_normalised(&c, ux, uy);
            let back_x = dx * c.fx + c.cx;
            let back_y = dy * c.fy + c.cy;
            assert!(
                (back_x - px).abs() < 0.5 && (back_y - py).abs() < 0.5,
                "({px},{py}) -> ({back_x},{back_y})"
            );
        }
    }

    #[test]
    fn barrel_distortion_compresses_the_edges() {
        // This is the bias the whole module exists to remove: the same plant at the
        // edge of an ultra-wide frame covers fewer pixels than one in the middle, so
        // its correction factor has to be larger.
        let c = wide();
        let centre = area_scale_at(&c, c.cx, c.cy);
        let edge = area_scale_at(&c, 1850.0, 1000.0);
        assert!(
            edge > centre * 1.2,
            "edge {edge} should need much more correction than centre {centre}"
        );
    }

    #[test]
    fn correction_is_symmetric_about_the_optical_centre() {
        let c = wide();
        let left = area_scale_at(&c, c.cx - 700.0, c.cy);
        let right = area_scale_at(&c, c.cx + 700.0, c.cy);
        assert!((left - right).abs() < 1e-4, "{left} vs {right}");
    }

    #[test]
    fn a_wild_calibration_cannot_produce_an_absurd_area() {
        let broken = LensCalibration {
            distortion: [-9.0, 40.0, 0.0, 0.0, 0.0],
            ..wide()
        };
        for x in [0.0, 500.0, 1919.0] {
            let scale = area_scale_at(&broken, x, 1079.0);
            assert!((MIN_SCALE..=MAX_SCALE).contains(&scale), "{scale}");
        }
    }
}
