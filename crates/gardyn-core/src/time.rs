//! Time helpers.
//!
//! The whole system reasons in fractional days, because that is the unit every
//! horticultural cadence is naturally expressed in ("check roots every 2-4 weeks",
//! "harvest 35 days after germination"). Keeping the arithmetic in `f64` days at the
//! domain layer avoids scattering unit conversions through the rules.

use jiff::Timestamp;

pub const SECONDS_PER_DAY: f64 = 86_400.0;

/// Fractional days from `earlier` to `later`. Negative if the arguments are reversed.
pub fn days_between(earlier: Timestamp, later: Timestamp) -> f64 {
    (later.as_second() - earlier.as_second()) as f64 / SECONDS_PER_DAY
}

/// Saturating offset by a fractional number of days.
pub fn add_days(t: Timestamp, days: f64) -> Timestamp {
    let seconds = t.as_second() as f64 + days * SECONDS_PER_DAY;
    // `as i64` saturates, so an out-of-range span clamps rather than wrapping.
    Timestamp::from_second(seconds.round() as i64).unwrap_or({
        if days >= 0.0 {
            Timestamp::MAX
        } else {
            Timestamp::MIN
        }
    })
}

pub fn add_hours(t: Timestamp, hours: f64) -> Timestamp {
    add_days(t, hours / 24.0)
}

/// Days elapsed since an event that may never have happened.
///
/// Returns infinity for `None`, which is deliberate: a cadence rule asking "has it
/// been more than 14 days since the last root check?" should fire for a planting
/// that has never been checked, without needing a special case at every call site.
pub fn days_since_or_never(then: Option<Timestamp>, now: Timestamp) -> f64 {
    match then {
        Some(t) => days_between(t, now),
        None => f64::INFINITY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    #[test]
    fn days_between_is_signed() {
        assert_eq!(days_between(ts(0), ts(86_400)), 1.0);
        assert_eq!(days_between(ts(86_400), ts(0)), -1.0);
        assert_eq!(days_between(ts(0), ts(43_200)), 0.5);
    }

    #[test]
    fn add_days_round_trips() {
        let base = ts(1_000_000);
        assert_eq!(days_between(base, add_days(base, 3.5)), 3.5);
        assert_eq!(days_between(base, add_days(base, -2.0)), -2.0);
    }

    #[test]
    fn add_days_saturates_instead_of_panicking() {
        assert_eq!(add_days(ts(0), 1e18), Timestamp::MAX);
        assert_eq!(add_days(ts(0), -1e18), Timestamp::MIN);
    }

    #[test]
    fn never_reads_as_infinitely_overdue() {
        assert_eq!(days_since_or_never(None, ts(0)), f64::INFINITY);
        assert_eq!(days_since_or_never(Some(ts(0)), ts(86_400)), 1.0);
    }
}
