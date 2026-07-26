//! Rendering a synthetic camera frame from simulator state.
//!
//! Real hardware does not exist yet, so a simulated garden draws its own photograph:
//! one blob per occupied slot, sized by the canopy area the physics model produced and
//! tinted by its chlorosis index. It is obviously not a photograph of a plant, and it
//! is not trying to be — its job is to make the capture, storage, authorization, and
//! display path real, so that swapping in a `/dev/video0` frame later changes one
//! function and nothing else.

use gardyn_core::{GardenState, SlotId};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use std::io::Cursor;

pub const FRAME_WIDTH: u32 = 640;
pub const FRAME_HEIGHT: u32 = 480;

/// Light duty the simulated camera pretends to capture at.
///
/// Fixed on purpose: this is the photo-mode reference level, so every synthetic frame
/// is photometrically comparable with every other one, exactly as real captures will
/// be once the edge agent pins the lights before shooting.
pub const REFERENCE_DUTY_MILLI: i64 = 800;

/// Render the garden as it would appear to the light-bar camera.
pub fn render(state: &GardenState) -> Result<Vec<u8>, image::ImageError> {
    let mut img = RgbImage::new(FRAME_WIDTH, FRAME_HEIGHT);
    paint_background(&mut img);

    let geometry = state.geometry;
    let columns = u32::from(geometry.columns.max(1));
    let rows = u32::from(geometry.rows_per_column.max(1));
    let cell_w = FRAME_WIDTH / columns;
    let cell_h = FRAME_HEIGHT / rows;

    for slot in geometry.slots() {
        let Some(position) = geometry.position(slot) else {
            continue;
        };
        let cx = u32::from(position.column) * cell_w + cell_w / 2;
        let cy = u32::from(position.row) * cell_h + cell_h / 2;

        match state.planting_in(slot) {
            Some(_) => {
                let (area, yellowing) = canopy_of(state, slot);
                // Radius from the square root of area, so a plant twice the area looks
                // twice the size rather than four times.
                let scale = (area.max(1.0)).sqrt() / 26.0;
                let rx = ((cell_w as f32 * 0.44) * scale.clamp(0.12, 1.0)) as i32;
                let ry = ((cell_h as f32 * 0.44) * scale.clamp(0.12, 1.0)) as i32;
                draw_canopy(&mut img, cx as i32, cy as i32, rx.max(3), ry.max(3), yellowing);
            }
            None => draw_empty_cup(&mut img, cx as i32, cy as i32),
        }
    }

    let mut buffer = Vec::new();
    DynamicImage::ImageRgb8(img).write_to(&mut Cursor::new(&mut buffer), ImageFormat::Png)?;
    Ok(buffer)
}

fn canopy_of(state: &GardenState, slot: SlotId) -> (f32, f32) {
    match state.metrics_for(slot) {
        Some(metrics) => (metrics.canopy_area_cm2, metrics.yellowing_index),
        // Vision is off for this garden, so fall back to a nominal size. The frame is
        // still worth capturing — the operator can look at it even when nothing is
        // measuring it.
        None => (180.0, 0.0),
    }
}

fn paint_background(img: &mut RgbImage) {
    let (w, h) = (img.width() as f32, img.height() as f32);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let max_distance = (cx * cx + cy * cy).sqrt();

    for (x, y, pixel) in img.enumerate_pixels_mut() {
        // Radial falloff, standing in for the light bar being brightest in the middle.
        let dx = x as f32 - cx;
        let dy = y as f32 - cy;
        let falloff = 1.0 - (dx * dx + dy * dy).sqrt() / max_distance * 0.55;
        let base = 26.0 * falloff.max(0.25);
        *pixel = Rgb([
            (base * 0.85) as u8,
            (base * 1.0) as u8,
            (base * 0.92) as u8,
        ]);
    }
}

fn draw_canopy(img: &mut RgbImage, cx: i32, cy: i32, rx: i32, ry: i32, yellowing: f32) {
    // Chlorosis shifts foliage from green toward yellow, which is what the vision
    // pipeline will eventually measure back out of a real frame.
    let y = yellowing.clamp(0.0, 1.0);
    let leaf = Rgb([
        (60.0 + 150.0 * y) as u8,
        (150.0 - 20.0 * y) as u8,
        (58.0 - 20.0 * y) as u8,
    ]);
    let highlight = Rgb([
        (leaf[0] as f32 * 1.35).min(255.0) as u8,
        (leaf[1] as f32 * 1.3).min(255.0) as u8,
        (leaf[2] as f32 * 1.3).min(255.0) as u8,
    ]);

    fill_ellipse(img, cx, cy, rx, ry, leaf);
    // A smaller, brighter core so the blob reads as lit from above rather than flat.
    fill_ellipse(
        img,
        cx - rx / 6,
        cy - ry / 6,
        (rx as f32 * 0.45) as i32,
        (ry as f32 * 0.45) as i32,
        highlight,
    );
}

fn draw_empty_cup(img: &mut RgbImage, cx: i32, cy: i32) {
    fill_ellipse(img, cx, cy, 9, 6, Rgb([54, 56, 50]));
    fill_ellipse(img, cx, cy, 6, 4, Rgb([32, 34, 30]));
}

fn fill_ellipse(img: &mut RgbImage, cx: i32, cy: i32, rx: i32, ry: i32, colour: Rgb<u8>) {
    if rx <= 0 || ry <= 0 {
        return;
    }
    let (w, h) = (img.width() as i32, img.height() as i32);

    for y in (cy - ry).max(0)..=(cy + ry).min(h - 1) {
        for x in (cx - rx).max(0)..=(cx + rx).min(w - 1) {
            let nx = (x - cx) as f32 / rx as f32;
            let ny = (y - cy) as f32 / ry as f32;
            let d = nx * nx + ny * ny;
            if d <= 1.0 {
                // Soften the rim so blobs do not look like clip art.
                let edge = ((1.0 - d) * 4.0).clamp(0.0, 1.0);
                let existing = img.get_pixel(x as u32, y as u32).0;
                let blended = Rgb([
                    blend(existing[0], colour[0], edge),
                    blend(existing[1], colour[1], edge),
                    blend(existing[2], colour[2], edge),
                ]);
                img.put_pixel(x as u32, y as u32, blended);
            }
        }
    }
}

fn blend(from: u8, to: u8, t: f32) -> u8 {
    (from as f32 * (1.0 - t) + to as f32 * t).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use gardyn_core::{Planting, PlantingId, SlotMetrics, Timestamp, VarietyId};
    use gardyn_store::frames::ImageKind;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn state_with_plants(count: u8, canopy: f32) -> GardenState {
        let mut state = GardenState::new_studio_2(t0());
        for i in 0..count {
            state.plantings.push(Planting::new(
                PlantingId(u64::from(i)),
                SlotId(i),
                VarietyId::new("kale-lacinato"),
                t0(),
            ));
            state
                .slot_metrics
                .insert(SlotId(i), SlotMetrics::new(SlotId(i), t0(), canopy));
        }
        state
    }

    #[test]
    fn a_rendered_frame_is_a_valid_png() {
        let bytes = render(&state_with_plants(4, 300.0)).unwrap();
        // The store sniffs magic bytes, so the renderer must satisfy that check.
        assert_eq!(ImageKind::sniff(&bytes), Some(ImageKind::Png));
        assert!(bytes.len() > 500, "suspiciously small: {} bytes", bytes.len());
    }

    #[test]
    fn an_empty_garden_still_renders() {
        let bytes = render(&GardenState::new_studio_2(t0())).unwrap();
        assert_eq!(ImageKind::sniff(&bytes), Some(ImageKind::Png));
    }

    #[test]
    fn a_fuller_garden_produces_a_different_image() {
        let sparse = render(&state_with_plants(2, 200.0)).unwrap();
        let full = render(&state_with_plants(12, 200.0)).unwrap();
        assert_ne!(sparse, full);
    }

    #[test]
    fn bigger_canopies_produce_a_different_image() {
        let small = render(&state_with_plants(4, 80.0)).unwrap();
        let large = render(&state_with_plants(4, 700.0)).unwrap();
        assert_ne!(small, large);
    }

    #[test]
    fn chlorosis_changes_the_colour() {
        // The colour shift the vision pipeline is eventually meant to detect.
        let mut healthy = state_with_plants(4, 400.0);
        let mut sick = state_with_plants(4, 400.0);
        for metrics in sick.slot_metrics.values_mut() {
            metrics.yellowing_index = 0.9;
        }
        for metrics in healthy.slot_metrics.values_mut() {
            metrics.yellowing_index = 0.0;
        }
        assert_ne!(render(&healthy).unwrap(), render(&sick).unwrap());
    }

    #[test]
    fn rendering_is_deterministic() {
        let state = state_with_plants(6, 350.0);
        assert_eq!(render(&state).unwrap(), render(&state).unwrap());
    }

    #[test]
    fn drawing_never_escapes_the_canvas() {
        // A huge canopy in a corner slot must clip, not panic.
        let mut state = state_with_plants(1, 100_000.0);
        state
            .slot_metrics
            .insert(SlotId(0), SlotMetrics::new(SlotId(0), t0(), 100_000.0));
        assert!(render(&state).is_ok());
    }
}
