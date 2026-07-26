//! Where each slot is in the frame, and how big a pixel is there.
//!
//! One ultra-wide camera on the light bar sees the whole tower, so a frame has to be
//! cut into per-slot rectangles before anything can be measured. That mapping is
//! physical — it depends on where the camera sits and how the tower is positioned —
//! so it is calibration data, not a constant.
//!
//! **The ROI map is also the on/off switch for vision.** A garden without one gets no
//! canopy metrics, which is not a limitation but a fact: there is no way to measure the
//! area of slot 7 without knowing which pixels are slot 7. `garden-cli vision
//! calibrate` is what produces one.

use garden_core::{Geometry, LensCalibration, SlotId};
use serde::{Deserialize, Serialize};

/// One slot's rectangle in the frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SlotRoi {
    pub slot: SlotId,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Real-world area of one pixel at this slot's distance from the camera, in cm².
    ///
    /// Per-slot rather than global because the tower recedes from the camera: a plant
    /// at the bottom of the column is further away and covers fewer pixels than the
    /// same plant at the top. Lens distortion is a separate correction — see
    /// [`crate::lens`] — because it varies across the frame rather than with distance.
    pub cm2_per_px: f32,
}

impl SlotRoi {
    pub fn centre(&self) -> (f32, f32) {
        (
            self.x as f32 + self.width as f32 / 2.0,
            self.y as f32 + self.height as f32 / 2.0,
        )
    }

    pub fn pixel_count(&self) -> u32 {
        self.width.saturating_mul(self.height)
    }

    /// Whether this rectangle fits inside a frame of the given size.
    pub fn fits(&self, frame_width: u32, frame_height: u32) -> bool {
        self.width > 0
            && self.height > 0
            && self.x + self.width <= frame_width
            && self.y + self.height <= frame_height
    }
}

/// Everything needed to turn one garden's frames into measurements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoiMap {
    /// Frame size this was calibrated against. A frame of a different size is rejected
    /// rather than scaled: a camera that changed resolution has almost certainly
    /// changed field of view too, and silently rescaling would produce confident
    /// wrong areas.
    pub frame_width: u32,
    pub frame_height: u32,
    pub lens: LensCalibration,
    pub slots: Vec<SlotRoi>,
    /// Where to look for algae. Usually the tank lid or the reservoir surface.
    #[serde(default)]
    pub tank: Option<Roi>,
    /// Whether `cm2_per_px` was measured, or is still the grid's placeholder.
    ///
    /// An explicit flag rather than "is the scale different from the default", which
    /// is what this was first: a perfectly ordinary calibration — a 7 cm yPod over 70
    /// pixels — lands on exactly the placeholder value, and the map then reported
    /// itself uncalibrated forever. A sentinel that a real measurement can collide
    /// with is not a sentinel.
    #[serde(default)]
    pub scale_measured: bool,
}

/// A bare rectangle, for regions that are not a slot.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Roi {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RoiError {
    #[error("frame is {got_w}×{got_h} but the map was calibrated for {want_w}×{want_h}")]
    WrongFrameSize {
        got_w: u32,
        got_h: u32,
        want_w: u32,
        want_h: u32,
    },
    #[error("slot {0} extends outside the frame")]
    OutOfBounds(SlotId),
    #[error("slot {0} appears more than once")]
    Duplicate(SlotId),
    #[error("the map has no slots")]
    Empty,
}

impl RoiMap {
    /// An evenly spaced grid, as a starting point for calibration.
    ///
    /// Never correct as-is — a real tower is not axis-aligned in the frame and the
    /// camera is not centred on it. This exists so that calibration is *adjusting
    /// numbers that are nearly right* rather than inventing sixty-four of them, and so
    /// `garden-cli vision calibrate` has something to draw on the first frame.
    ///
    /// `margin` is the fraction of each cell left as a gutter, which keeps neighbouring
    /// plants from leaning into each other's rectangle.
    pub fn grid(
        geometry: &Geometry,
        frame_width: u32,
        frame_height: u32,
        margin: f32,
    ) -> Self {
        let columns = u32::from(geometry.columns.max(1));
        let rows = u32::from(geometry.rows_per_column.max(1));
        let cell_w = frame_width / columns;
        let cell_h = frame_height / rows;
        let margin = margin.clamp(0.0, 0.4);
        let gutter_x = (cell_w as f32 * margin) as u32;
        let gutter_y = (cell_h as f32 * margin) as u32;

        let mut slots = Vec::new();
        for slot in geometry.slots() {
            let Some(position) = geometry.position(slot) else {
                continue;
            };
            // `SlotPosition` is 0-based, column-major.
            let column = u32::from(position.column);
            let row = u32::from(position.row);
            let width = cell_w.saturating_sub(gutter_x * 2).max(1);
            let height = cell_h.saturating_sub(gutter_y * 2).max(1);
            slots.push(SlotRoi {
                slot,
                x: column * cell_w + gutter_x,
                y: row * cell_h + gutter_y,
                width,
                height,
                // A placeholder scale. Calibration replaces it by measuring something
                // of known size in the frame; until then areas are in the right shape
                // but the wrong units, which is why `is_calibrated` exists.
                cm2_per_px: DEFAULT_CM2_PER_PX,
            });
        }

        Self {
            frame_width,
            frame_height,
            lens: LensCalibration::IDENTITY,
            slots,
            tank: None,
            scale_measured: false,
        }
    }

    /// Whether the scale has been measured, or is still the grid placeholder.
    ///
    /// Areas from an uncalibrated map are comparable *to each other over time* — which
    /// is enough for growth rate and for stall detection — but their absolute value is
    /// meaningless, so the harvest threshold must not trust them.
    pub fn is_calibrated(&self) -> bool {
        self.scale_measured
    }

    pub fn get(&self, slot: SlotId) -> Option<&SlotRoi> {
        self.slots.iter().find(|s| s.slot == slot)
    }

    /// Check the map is usable against a frame of this size.
    pub fn validate(&self, frame_width: u32, frame_height: u32) -> Result<(), RoiError> {
        if self.slots.is_empty() {
            return Err(RoiError::Empty);
        }
        if frame_width != self.frame_width || frame_height != self.frame_height {
            return Err(RoiError::WrongFrameSize {
                got_w: frame_width,
                got_h: frame_height,
                want_w: self.frame_width,
                want_h: self.frame_height,
            });
        }
        let mut seen = Vec::with_capacity(self.slots.len());
        for roi in &self.slots {
            if seen.contains(&roi.slot) {
                return Err(RoiError::Duplicate(roi.slot));
            }
            seen.push(roi.slot);
            if !roi.fits(frame_width, frame_height) {
                return Err(RoiError::OutOfBounds(roi.slot));
            }
        }
        Ok(())
    }
}

/// The grid placeholder scale, and the marker for "not yet measured".
pub const DEFAULT_CM2_PER_PX: f32 = 0.01;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grid_covers_every_slot_exactly_once() {
        let map = RoiMap::grid(&Geometry::STUDIO_2, 1920, 1080, 0.1);
        assert_eq!(map.slots.len(), 16);
        assert!(map.validate(1920, 1080).is_ok());
    }

    #[test]
    fn grid_rectangles_stay_inside_the_frame() {
        for (w, h) in [(1920, 1080), (640, 480), (1280, 720)] {
            let map = RoiMap::grid(&Geometry::STUDIO_2, w, h, 0.15);
            for roi in &map.slots {
                assert!(roi.fits(w, h), "{roi:?} escapes {w}×{h}");
            }
        }
    }

    #[test]
    fn a_grid_places_columns_side_by_side_and_rows_top_to_bottom() {
        let map = RoiMap::grid(&Geometry::STUDIO_2, 1920, 1080, 0.0);
        let first = map.get(SlotId(0)).unwrap();
        let second_row = map.get(SlotId(1)).unwrap();
        let other_column = map.get(SlotId(8)).unwrap();

        assert_eq!(first.x, second_row.x, "same column, same x");
        assert!(second_row.y > first.y, "next slot is below");
        assert!(other_column.x > first.x, "the second column is to the right");
    }

    #[test]
    fn a_frame_of_the_wrong_size_is_rejected_rather_than_rescaled() {
        // A camera whose resolution changed has almost certainly changed field of
        // view. Scaling the rectangles would produce confident, wrong areas.
        let map = RoiMap::grid(&Geometry::STUDIO_2, 1920, 1080, 0.1);
        assert_eq!(
            map.validate(1280, 720),
            Err(RoiError::WrongFrameSize {
                got_w: 1280,
                got_h: 720,
                want_w: 1920,
                want_h: 1080
            })
        );
    }

    #[test]
    fn a_duplicated_slot_is_caught() {
        let mut map = RoiMap::grid(&Geometry::STUDIO_2, 1920, 1080, 0.1);
        map.slots[3].slot = map.slots[0].slot;
        assert_eq!(map.validate(1920, 1080), Err(RoiError::Duplicate(SlotId(0))));
    }

    #[test]
    fn an_escaping_rectangle_is_caught() {
        let mut map = RoiMap::grid(&Geometry::STUDIO_2, 1920, 1080, 0.1);
        map.slots[5].width = 4000;
        assert!(matches!(
            map.validate(1920, 1080),
            Err(RoiError::OutOfBounds(_))
        ));
    }

    #[test]
    fn a_fresh_grid_knows_it_has_no_real_scale() {
        let mut map = RoiMap::grid(&Geometry::STUDIO_2, 1920, 1080, 0.1);
        assert!(!map.is_calibrated());
        map.scale_measured = true;
        assert!(map.is_calibrated());
    }

    #[test]
    fn a_measurement_that_lands_on_the_placeholder_value_still_counts() {
        // 7 cm over 70 px is 0.01 cm² per pixel, which is exactly the grid's
        // placeholder. Inferring "calibrated" from the value would call this map
        // uncalibrated and quietly refuse to report real areas.
        let mut map = RoiMap::grid(&Geometry::STUDIO_2, 1920, 1080, 0.1);
        for slot in &mut map.slots {
            slot.cm2_per_px = DEFAULT_CM2_PER_PX;
        }
        map.scale_measured = true;
        assert!(map.is_calibrated());
    }

    #[test]
    fn a_map_round_trips_through_json() {
        let map = RoiMap::grid(&Geometry::STUDIO_2, 1920, 1080, 0.1);
        let text = serde_json::to_string(&map).unwrap();
        assert_eq!(serde_json::from_str::<RoiMap>(&text).unwrap(), map);
    }
}
