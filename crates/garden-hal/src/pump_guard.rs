//! A hard ceiling on how long the pump may run continuously.
//!
//! This is the safeguard that replaced the 30% duty cap. That cap came from the Home
//! line and never applied here: the Studio 2's pump is a plain digital output the
//! factory runs flat out, so a fractional ceiling bounded nothing while reading as
//! though it did.
//!
//! **The factory's own bound is on-time, and this is a copy of it.** `main.py` runs a
//! `thread_pump_guardian` that waits for an `ON`, then forces the pump off if no `OFF`
//! arrives within `MAX_WATER_TIME` — fifteen minutes, three times the longest scheduled
//! run. Gardyn thought a stuck pump was worth guarding against on hardware they
//! designed; we are guarding against the same thing on hardware we did not.
//!
//! It sits below the schedule deliberately. `Schedule::validate` will accept
//! `pump_on_minutes: 360`, and a schedule arrives over the network from a machine that
//! could be wrong, compromised, or simply mid-deploy. This is the layer that does not
//! care what it was asked for.
//!
//! Monotonic, not wall-clock. The Pi has no RTC and `systemd-timesyncd` steps the clock
//! once the network comes up; a backwards step measured against wall time would extend
//! a run rather than end it.

use crate::Duty;
use std::time::{Duration, Instant};

/// The longest the pump may run without a break.
///
/// `MAX_WATER_TIME` in the factory's `config.py`, and generous against a stock cycle of
/// five minutes: this is a backstop for a schedule or a process that has gone wrong,
/// not a limit anything should reach in normal use.
pub const MAX_RUN: Duration = Duration::from_secs(15 * 60);

/// Tracks one pump's continuous run and cuts it short if it goes on too long.
#[derive(Debug, Default)]
pub struct PumpGuard {
    /// When the current run began, if it is running.
    started: Option<Instant>,
    /// Latched once a run has been cut, cleared only by an explicit request for off.
    ///
    /// Without this the guard would chatter: cut the pump, see the schedule still
    /// asking for it a second later, and start a fresh run. The factory's guardian has
    /// the same shape — it forces off and then waits for the next `OFF` signal.
    tripped: bool,
}

impl PumpGuard {
    /// `const` so the failsafe can hold one in a `static` without a lazy initialiser —
    /// one less moving part in the process whose whole job is having few of them.
    pub const fn new() -> Self {
        Self {
            started: None,
            tripped: false,
        }
    }

    /// What the pump is actually allowed to do, given what was asked for.
    ///
    /// Returns [`Duty::OFF`] once a run has exceeded [`MAX_RUN`], and keeps returning it
    /// until something asks for off — at which point the guard rearms.
    pub fn allow(&mut self, requested: Duty, now: Instant) -> Duty {
        if requested.is_off() {
            self.started = None;
            self.tripped = false;
            return Duty::OFF;
        }
        if self.tripped {
            return Duty::OFF;
        }

        let started = *self.started.get_or_insert(now);
        // `saturating_duration_since` rather than subtraction: `Instant` is monotonic,
        // but a caller passing an older instant than the one it passed last should get a
        // zero rather than a panic.
        if now.saturating_duration_since(started) >= MAX_RUN {
            self.tripped = true;
            return Duty::OFF;
        }
        requested
    }

    /// Whether the guard has cut a run short and is holding the pump off.
    ///
    /// Worth surfacing rather than hiding: a pump that keeps hitting this is either a
    /// bad schedule or a stuck valve, and both want a person to know.
    pub fn has_tripped(&self) -> bool {
        self.tripped
    }

    /// How long the current run has been going, if one is.
    pub fn running_for(&self, now: Instant) -> Option<Duration> {
        self.started.map(|s| now.saturating_duration_since(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> Duty {
        Duty::FULL
    }

    #[test]
    fn a_normal_cycle_passes_through_untouched() {
        // Five minutes is the factory's scheduled run, and nothing about it should
        // notice the guard exists.
        let mut guard = PumpGuard::new();
        let t0 = Instant::now();
        for minute in 0..5 {
            let allowed = guard.allow(on(), t0 + Duration::from_secs(minute * 60));
            assert_eq!(allowed, on(), "cut short at minute {minute}");
        }
        assert!(!guard.has_tripped());
    }

    #[test]
    fn a_run_past_the_ceiling_is_cut() {
        let mut guard = PumpGuard::new();
        let t0 = Instant::now();
        assert_eq!(guard.allow(on(), t0), on());
        assert_eq!(guard.allow(on(), t0 + MAX_RUN - Duration::from_secs(1)), on());
        assert_eq!(guard.allow(on(), t0 + MAX_RUN), Duty::OFF);
        assert!(guard.has_tripped());
    }

    #[test]
    fn a_schedule_that_asks_for_six_hours_gets_fifteen_minutes() {
        // The gap this exists to close. `Schedule::validate` accepts
        // `pump_on_minutes: 360`, and on binary hardware that is six hours of pumping.
        let mut guard = PumpGuard::new();
        let t0 = Instant::now();
        let mut pumped = Duration::ZERO;
        for second in (0..6 * 3600).step_by(30) {
            let at = t0 + Duration::from_secs(second);
            if !guard.allow(on(), at).is_off() {
                pumped += Duration::from_secs(30);
            }
        }
        assert!(
            pumped <= MAX_RUN,
            "pumped for {pumped:?}, which is past the ceiling"
        );
    }

    #[test]
    fn the_guard_does_not_chatter_once_it_has_tripped() {
        // A guard that rearmed while the schedule was still asking would cut the pump
        // and immediately restart it, which is a fifteen-minute duty cycle rather than
        // a stop.
        let mut guard = PumpGuard::new();
        let t0 = Instant::now();
        guard.allow(on(), t0);
        assert_eq!(guard.allow(on(), t0 + MAX_RUN), Duty::OFF);
        for extra in [1, 60, 3600] {
            let at = t0 + MAX_RUN + Duration::from_secs(extra);
            assert_eq!(guard.allow(on(), at), Duty::OFF, "restarted after {extra}s");
        }
    }

    #[test]
    fn asking_for_off_rearms_it_for_the_next_cycle() {
        // The factory's guardian waits for the next `OFF` before it will let the pump
        // run again, and so does this. Otherwise one bad run would disable watering
        // until the process restarted.
        let mut guard = PumpGuard::new();
        let t0 = Instant::now();
        guard.allow(on(), t0);
        assert_eq!(guard.allow(on(), t0 + MAX_RUN), Duty::OFF);

        assert_eq!(guard.allow(Duty::OFF, t0 + MAX_RUN), Duty::OFF);
        assert!(!guard.has_tripped());

        let next = t0 + MAX_RUN + Duration::from_secs(60);
        assert_eq!(guard.allow(on(), next), on(), "the next cycle should run");
    }

    #[test]
    fn a_gap_between_runs_resets_the_clock() {
        // Fourteen minutes on, off, fourteen minutes on is two legitimate runs, not one
        // twenty-eight minute run that should have been cut.
        let mut guard = PumpGuard::new();
        let t0 = Instant::now();
        let nearly = MAX_RUN - Duration::from_secs(60);

        guard.allow(on(), t0);
        assert_eq!(guard.allow(on(), t0 + nearly), on());
        guard.allow(Duty::OFF, t0 + nearly);

        let second_run = t0 + nearly + Duration::from_secs(60);
        assert_eq!(guard.allow(on(), second_run), on());
        assert_eq!(guard.allow(on(), second_run + nearly), on());
    }

    #[test]
    fn it_reports_how_long_the_pump_has_been_going() {
        let mut guard = PumpGuard::new();
        let t0 = Instant::now();
        assert_eq!(guard.running_for(t0), None);
        guard.allow(on(), t0);
        assert_eq!(
            guard.running_for(t0 + Duration::from_secs(90)),
            Some(Duration::from_secs(90))
        );
        guard.allow(Duty::OFF, t0 + Duration::from_secs(90));
        assert_eq!(guard.running_for(t0 + Duration::from_secs(90)), None);
    }
}
