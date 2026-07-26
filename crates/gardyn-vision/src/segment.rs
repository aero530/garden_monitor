//! Stage B: counting distinct seedlings, and spotting flowers.
//!
//! Thinning is the task this exists for. Gardyn's yCubes germinate several seeds and
//! the surplus has to be pinched out; "how many are up in slot 4" is a question a
//! calendar cannot answer, and getting it wrong in either direction costs a crop —
//! thin too early and you remove the survivor, too late and none of them size up.
//!
//! Two implementations behind one trait. The default counts connected components in
//! the canopy mask, which needs no model, no download, and no inference runtime. An
//! ONNX-backed segmenter slots in behind the same trait when a model is worth its
//! weight; the point of the trait is that nothing downstream changes when it does.

use crate::canopy::Mask;

/// What stage B adds to a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Segmentation {
    /// Distinct plants found. `None` when the mask was too sparse to judge.
    pub plant_count: Option<u8>,
    pub flowering: Option<bool>,
}

pub trait Segmenter: Send + Sync {
    /// Count plants in a canopy mask.
    ///
    /// `flower_pixels` is the count of pixels in the ROI whose hue fell outside both
    /// foliage bands while still being saturated and lit — petals, in other words.
    /// Passed in rather than recomputed because the classification pass already
    /// visited every pixel.
    fn segment(&self, mask: &Mask, flower_pixels: u32) -> Segmentation;
}

/// Connected-component counting. No model, no inference, no download.
///
/// Honest about what it is: two seedlings whose leaves overlap are one component and
/// will be counted as one plant. That failure mode is the *safe* direction — it
/// under-counts, so the thinning task fires late rather than telling you to pull a
/// plant that is not there.
#[derive(Debug, Clone, Copy)]
pub struct ConnectedComponents {
    /// Components smaller than this fraction of the largest are noise: a speck of
    /// algae on the yPod, a leaf tip from the neighbouring slot leaning in.
    pub min_relative_size: f32,
    /// Absolute floor, in pixels, below which nothing counts.
    pub min_pixels: u32,
    /// Petal pixels as a fraction of canopy above which the plant is flowering.
    pub flowering_fraction: f32,
}

impl Default for ConnectedComponents {
    fn default() -> Self {
        Self {
            min_relative_size: 0.15,
            min_pixels: 40,
            flowering_fraction: 0.04,
        }
    }
}

impl Segmenter for ConnectedComponents {
    fn segment(&self, mask: &Mask, flower_pixels: u32) -> Segmentation {
        let canopy = mask.count();
        if canopy < self.min_pixels {
            // Nothing has come up yet, or the frame was unusable. Either way, refusing
            // to answer is better than reporting zero plants — zero would read as
            // "germination failed" and raise a task.
            return Segmentation::default();
        }

        let mut sizes = component_sizes(mask);
        sizes.sort_unstable_by(|a, b| b.cmp(a));
        let largest = sizes.first().copied().unwrap_or(0);
        let floor = self
            .min_pixels
            .max((largest as f32 * self.min_relative_size) as u32);

        let count = sizes.iter().filter(|s| **s >= floor).count();

        Segmentation {
            plant_count: Some(count.clamp(1, u8::MAX as usize) as u8),
            flowering: Some(flower_pixels as f32 / canopy as f32 >= self.flowering_fraction),
        }
    }
}

/// Sizes of every 4-connected component in the mask.
///
/// Iterative flood fill with an explicit stack. A recursive version blows the stack on
/// a mask where one plant fills the rectangle, which is exactly the case that matters.
fn component_sizes(mask: &Mask) -> Vec<u32> {
    let width = mask.width as usize;
    let height = mask.height as usize;
    let mut seen = vec![false; width * height];
    let mut sizes = Vec::new();
    let mut stack: Vec<(u32, u32)> = Vec::new();

    for y in 0..mask.height {
        for x in 0..mask.width {
            let start = (y as usize) * width + (x as usize);
            if seen[start] || !mask.get(x, y) {
                continue;
            }
            let mut size = 0u32;
            stack.push((x, y));
            seen[start] = true;

            while let Some((cx, cy)) = stack.pop() {
                size += 1;
                let neighbours = [
                    (cx.wrapping_sub(1), cy),
                    (cx + 1, cy),
                    (cx, cy.wrapping_sub(1)),
                    (cx, cy + 1),
                ];
                for (nx, ny) in neighbours {
                    if nx >= mask.width || ny >= mask.height || !mask.get(nx, ny) {
                        continue;
                    }
                    let index = (ny as usize) * width + (nx as usize);
                    if !seen[index] {
                        seen[index] = true;
                        stack.push((nx, ny));
                    }
                }
            }
            sizes.push(size);
        }
    }
    let _ = height;
    sizes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask_with(width: u32, height: u32, blobs: &[(u32, u32, u32, u32)]) -> Mask {
        let mut mask = Mask::new(width, height);
        for (x, y, w, h) in blobs {
            for dy in 0..*h {
                for dx in 0..*w {
                    mask.set(x + dx, y + dy, true);
                }
            }
        }
        mask
    }

    #[test]
    fn one_blob_is_one_plant() {
        let mask = mask_with(100, 100, &[(10, 10, 30, 30)]);
        let seg = ConnectedComponents::default().segment(&mask, 0);
        assert_eq!(seg.plant_count, Some(1));
    }

    #[test]
    fn three_separated_seedlings_are_counted() {
        // The case thinning depends on: several sprouts, still small, not yet touching.
        let mask = mask_with(100, 100, &[(5, 5, 12, 12), (40, 5, 12, 12), (75, 5, 12, 12)]);
        let seg = ConnectedComponents::default().segment(&mask, 0);
        assert_eq!(seg.plant_count, Some(3));
    }

    #[test]
    fn touching_seedlings_undercount_rather_than_overcount() {
        // Two plants whose leaves overlap read as one. That is the safe direction:
        // thinning fires late instead of telling you to pull a plant that is not there.
        let mask = mask_with(100, 100, &[(10, 10, 20, 20), (29, 10, 20, 20)]);
        let seg = ConnectedComponents::default().segment(&mask, 0);
        assert_eq!(seg.plant_count, Some(1));
    }

    #[test]
    fn specks_are_not_plants() {
        let mask = mask_with(
            100,
            100,
            &[(10, 10, 30, 30), (80, 80, 3, 3), (90, 10, 2, 2)],
        );
        let seg = ConnectedComponents::default().segment(&mask, 0);
        assert_eq!(seg.plant_count, Some(1));
    }

    #[test]
    fn a_leaf_leaning_in_from_next_door_is_ignored() {
        // Small relative to the plant that owns the slot, so it falls under the
        // relative-size floor even though it is well above the absolute one.
        let mask = mask_with(200, 200, &[(20, 20, 100, 100), (180, 20, 8, 8)]);
        let seg = ConnectedComponents::default().segment(&mask, 0);
        assert_eq!(seg.plant_count, Some(1));
    }

    #[test]
    fn an_empty_slot_declines_to_answer_rather_than_reporting_zero() {
        // Zero plants would read as "germination failed" and raise a task. The honest
        // answer for an unusable mask is that there is no answer.
        let seg = ConnectedComponents::default().segment(&Mask::new(100, 100), 0);
        assert_eq!(seg.plant_count, None);
        assert_eq!(seg.flowering, None);
    }

    #[test]
    fn petals_above_the_threshold_mean_flowering() {
        let mask = mask_with(100, 100, &[(10, 10, 40, 40)]);
        let canopy = mask.count();
        let sut = ConnectedComponents::default();
        assert_eq!(sut.segment(&mask, canopy / 100).flowering, Some(false));
        assert_eq!(sut.segment(&mask, canopy / 10).flowering, Some(true));
    }

    #[test]
    fn a_mask_that_is_entirely_canopy_does_not_blow_the_stack() {
        // The flood fill is iterative for exactly this case: one plant that has filled
        // its whole rectangle. A recursive version dies here.
        let mut mask = Mask::new(300, 300);
        for y in 0..300 {
            for x in 0..300 {
                mask.set(x, y, true);
            }
        }
        assert_eq!(ConnectedComponents::default().segment(&mask, 0).plant_count, Some(1));
    }

    #[test]
    fn a_diagonal_chain_is_four_connected_not_eight() {
        // Documenting the choice: diagonal touching does not join components. Eight
        // connectivity merges seedlings that are merely near each other, and merging
        // is the failure that causes under-thinning.
        let mut mask = Mask::new(50, 50);
        for i in 0..10 {
            for dy in 0..4 {
                for dx in 0..4 {
                    mask.set(i * 5 + dx, i * 5 + dy, true);
                }
            }
        }
        let count = ConnectedComponents {
            min_pixels: 4,
            min_relative_size: 0.1,
            ..Default::default()
        }
        .segment(&mask, 0)
        .plant_count
        .unwrap();
        assert!(count > 1, "diagonal blobs should stay separate, got {count}");
    }
}
