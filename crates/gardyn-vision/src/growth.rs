//! Fitting a growth rate from a slot's measurement history.
//!
//! A single frame says how big a plant is. Two weeks of frames say whether it is still
//! growing, and that is the more useful signal — a stalled canopy means stress, root
//! binding, or simply that the plant is finished, and none of those are visible in an
//! absolute area.

use jiff::Timestamp;

/// One historical measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub at: Timestamp,
    pub area_cm2: f32,
}

/// Ordinary least-squares slope of area against time, in cm² per day.
///
/// Returns `None` when there is not enough spread to fit anything — one sample, or
/// several from the same afternoon. Returning zero instead would be indistinguishable
/// from a genuinely stalled plant, and stalling drives a task.
pub fn fit_rate(samples: &[Sample]) -> Option<f32> {
    if samples.len() < MIN_SAMPLES {
        return None;
    }

    // Days relative to the first sample, so the numbers stay small and `f64` keeps its
    // precision. Unix seconds squared overflows the useful range of an f32 fit.
    let origin = samples[0].at;
    let points: Vec<(f64, f64)> = samples
        .iter()
        .map(|s| {
            (
                gardyn_core::time::days_between(origin, s.at),
                f64::from(s.area_cm2),
            )
        })
        .collect();

    let n = points.len() as f64;
    let mean_x = points.iter().map(|p| p.0).sum::<f64>() / n;
    let mean_y = points.iter().map(|p| p.1).sum::<f64>() / n;

    let mut covariance = 0.0;
    let mut variance = 0.0;
    for (x, y) in &points {
        covariance += (x - mean_x) * (y - mean_y);
        variance += (x - mean_x) * (x - mean_x);
    }

    if variance < MIN_SPREAD_DAYS_SQUARED {
        return None;
    }
    let slope = covariance / variance;
    slope.is_finite().then_some(slope as f32)
}

/// Fewer than this and a line through the points means nothing.
const MIN_SAMPLES: usize = 3;
/// Roughly a day of spread, squared. Below this the samples are effectively
/// simultaneous and the slope is dominated by measurement noise.
const MIN_SPREAD_DAYS_SQUARED: f64 = 0.5;

/// Keep only samples inside the trailing window, oldest first.
///
/// The window matters: too short and normal day-to-day noise reads as a stall, too long
/// and a plant that stopped growing last week is still averaged against the fortnight
/// when it was thriving.
pub fn window(samples: &[Sample], now: Timestamp, days: f64) -> Vec<Sample> {
    let cutoff = gardyn_core::time::add_days(now, -days);
    let mut kept: Vec<Sample> = samples.iter().copied().filter(|s| s.at >= cutoff).collect();
    kept.sort_by_key(|s| s.at);
    kept
}

/// The default trailing window for growth fitting.
pub const WINDOW_DAYS: f64 = 10.0;

#[cfg(test)]
mod tests {
    use super::*;
    use gardyn_core::time::add_days;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn series(areas: &[(f64, f32)]) -> Vec<Sample> {
        areas
            .iter()
            .map(|(day, area)| Sample {
                at: add_days(t0(), *day),
                area_cm2: *area,
            })
            .collect()
    }

    #[test]
    fn steady_growth_recovers_its_slope() {
        let samples = series(&[(0.0, 100.0), (1.0, 110.0), (2.0, 120.0), (3.0, 130.0)]);
        let rate = fit_rate(&samples).unwrap();
        assert!((rate - 10.0).abs() < 0.01, "{rate}");
    }

    #[test]
    fn a_shrinking_canopy_gives_a_negative_rate() {
        let samples = series(&[(0.0, 300.0), (2.0, 280.0), (4.0, 250.0), (6.0, 230.0)]);
        let rate = fit_rate(&samples).unwrap();
        assert!(rate < -10.0, "{rate}");
    }

    #[test]
    fn noise_around_a_flat_line_reads_as_stalled_not_growing() {
        let samples = series(&[
            (0.0, 300.0),
            (1.0, 297.0),
            (2.0, 304.0),
            (3.0, 299.0),
            (4.0, 301.0),
        ]);
        let rate = fit_rate(&samples).unwrap();
        assert!(rate.abs() < 0.5, "{rate}");
    }

    #[test]
    fn too_few_samples_is_none_rather_than_zero() {
        // Zero would be indistinguishable from a stalled plant, and stalling raises a
        // task. "I do not know yet" has to be its own answer.
        assert_eq!(fit_rate(&[]), None);
        assert_eq!(fit_rate(&series(&[(0.0, 100.0)])), None);
        assert_eq!(fit_rate(&series(&[(0.0, 100.0), (1.0, 120.0)])), None);
    }

    #[test]
    fn samples_from_one_afternoon_cannot_be_fitted() {
        let samples = series(&[(0.0, 100.0), (0.04, 103.0), (0.08, 99.0)]);
        assert_eq!(fit_rate(&samples), None);
    }

    #[test]
    fn one_outlier_does_not_dominate_the_fit() {
        // A single bad frame — a hand in shot, a mistimed capture — should bend the
        // line, not define it.
        let clean = series(&[(0.0, 100.0), (2.0, 120.0), (4.0, 140.0), (6.0, 160.0)]);
        let mut dirty = clean.clone();
        dirty.push(Sample {
            at: add_days(t0(), 3.0),
            area_cm2: 20.0,
        });
        dirty.sort_by_key(|s| s.at);

        let clean_rate = fit_rate(&clean).unwrap();
        let dirty_rate = fit_rate(&dirty).unwrap();
        assert!(dirty_rate > clean_rate * 0.4, "{dirty_rate} vs {clean_rate}");
    }

    #[test]
    fn the_window_drops_old_samples_and_sorts_the_rest() {
        let now = add_days(t0(), 30.0);
        let samples = series(&[(0.0, 10.0), (28.0, 90.0), (25.0, 80.0), (29.0, 95.0)]);
        let kept = window(&samples, now, 10.0);
        assert_eq!(kept.len(), 3);
        assert!(kept.windows(2).all(|w| w[0].at <= w[1].at));
        assert_eq!(kept[0].area_cm2, 80.0);
    }
}
