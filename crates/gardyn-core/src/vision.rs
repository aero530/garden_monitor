//! Per-slot metrics derived from the camera.
//!
//! The Studio 2 has a single ultra-wide camera on the light bar, so one frame is
//! undistorted and then split into per-slot regions of interest. Fields here are
//! grouped by which vision capability produces them, and each group is `Option`
//! because the three stages are independently switchable.

use crate::slot::SlotId;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotMetrics {
    pub slot: SlotId,
    pub at: Timestamp,

    // --- Capability::CanopyMetrics (HSV masking, no ML) -------------------------
    /// Projected canopy area after lens undistortion.
    pub canopy_area_cm2: f32,
    /// Fraction of the ROI classified as foliage.
    pub green_fraction: f32,
    /// 0.0 healthy green through 1.0 fully chlorotic. A nitrogen-deficiency proxy.
    pub yellowing_index: f32,
    /// Fitted growth rate over the recent window. Negative means the plant is losing
    /// canopy, which is a stronger distress signal than any absolute measure.
    pub growth_rate_cm2_per_day: f32,

    // --- Capability::PlantSegmentation (ONNX) -----------------------------------
    /// Distinct seedlings detected, used to drive thinning.
    pub plant_count: Option<u8>,
    pub flowering: Option<bool>,

    // --- Capability::VisualDiagnosis (local VLM) --------------------------------
    /// Free-text assessment. Advisory only: never allowed to trigger dosing.
    pub diagnosis: Option<String>,
}

impl SlotMetrics {
    pub fn new(slot: SlotId, at: Timestamp, canopy_area_cm2: f32) -> Self {
        Self {
            slot,
            at,
            canopy_area_cm2,
            green_fraction: 0.0,
            yellowing_index: 0.0,
            growth_rate_cm2_per_day: 0.0,
            plant_count: None,
            flowering: None,
            diagnosis: None,
        }
    }

    /// Canopy has stopped expanding, suggesting the plant is stressed, root-bound, or
    /// simply finished.
    pub fn is_stalled(&self) -> bool {
        self.growth_rate_cm2_per_day < Self::STALL_THRESHOLD
    }

    pub fn is_chlorotic(&self) -> bool {
        self.yellowing_index >= Self::CHLOROSIS_THRESHOLD
    }

    /// Growth below this is indistinguishable from measurement noise.
    const STALL_THRESHOLD: f32 = 0.5;
    const CHLOROSIS_THRESHOLD: f32 = 0.35;
}

/// Algae and biofilm coverage observed on the tank lid and column surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AlgaeReading {
    pub at: Timestamp,
    /// Fraction of inspected surface showing growth.
    pub coverage: f32,
}

impl AlgaeReading {
    /// Worth an extra dose of conditioner.
    pub const ADVISORY_COVERAGE: f32 = 0.10;
    /// Worth a tank refresh ahead of schedule.
    pub const URGENT_COVERAGE: f32 = 0.25;

    pub fn is_advisory(&self) -> bool {
        self.coverage >= Self::ADVISORY_COVERAGE
    }

    pub fn is_urgent(&self) -> bool {
        self.coverage >= Self::URGENT_COVERAGE
    }
}

/// Lens calibration for the ultra-wide camera.
///
/// Undistortion is not optional. Barrel distortion on an ultra-wide lens shrinks
/// slots toward the frame edges, so without correcting for it the outer columns would
/// read as systematically smaller plants and the harvest rule would under-fire on them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LensCalibration {
    pub fx: f32,
    pub fy: f32,
    pub cx: f32,
    pub cy: f32,
    /// Radial and tangential coefficients, OpenCV ordering: k1, k2, p1, p2, k3.
    pub distortion: [f32; 5],
}

impl LensCalibration {
    /// Identity calibration, for the simulator and for tests.
    pub const IDENTITY: LensCalibration = LensCalibration {
        fx: 1.0,
        fy: 1.0,
        cx: 0.0,
        cy: 0.0,
        distortion: [0.0; 5],
    };

    pub fn is_identity(&self) -> bool {
        self.distortion.iter().all(|c| c.abs() < f32::EPSILON)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    #[test]
    fn optional_stages_default_to_absent() {
        let m = SlotMetrics::new(SlotId(0), t0(), 100.0);
        assert_eq!(m.plant_count, None);
        assert_eq!(m.flowering, None);
        assert_eq!(m.diagnosis, None);
    }

    #[test]
    fn shrinking_canopy_counts_as_stalled() {
        let mut m = SlotMetrics::new(SlotId(0), t0(), 300.0);
        m.growth_rate_cm2_per_day = 4.0;
        assert!(!m.is_stalled());
        m.growth_rate_cm2_per_day = -2.0;
        assert!(m.is_stalled());
    }

    #[test]
    fn chlorosis_has_a_threshold() {
        let mut m = SlotMetrics::new(SlotId(0), t0(), 300.0);
        m.yellowing_index = 0.2;
        assert!(!m.is_chlorotic());
        m.yellowing_index = 0.5;
        assert!(m.is_chlorotic());
    }

    #[test]
    fn algae_escalates_in_two_steps() {
        let mild = AlgaeReading {
            at: t0(),
            coverage: 0.15,
        };
        let bad = AlgaeReading {
            at: t0(),
            coverage: 0.30,
        };
        assert!(mild.is_advisory() && !mild.is_urgent());
        assert!(bad.is_advisory() && bad.is_urgent());
    }

    #[test]
    fn identity_calibration_is_recognised() {
        assert!(LensCalibration::IDENTITY.is_identity());
        let real = LensCalibration {
            distortion: [-0.28, 0.09, 0.0, 0.0, 0.0],
            ..LensCalibration::IDENTITY
        };
        assert!(!real.is_identity());
    }
}
