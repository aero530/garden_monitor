//! The resident schedule: what the lights and pump should be doing right now.
//!
//! **The brain is never in the control loop.** This lives on the Pi and runs from the
//! Pi's own clock. The brain pushes a new schedule occasionally; it never issues
//! per-cycle commands. If the LAN dies, the VM dies, or Proxmox reboots for a kernel
//! update, the garden keeps growing on the last schedule it was given. That is
//! non-negotiable once the vendor firmware is gone, because there is nothing else left
//! to keep the plants alive.
//!
//! A pure function of seconds-since-midnight, deliberately. No clock, no state, no
//! I/O — so the whole day can be checked in a test, and a timezone mistake cannot make
//! the lights come on at three in the morning without a test noticing.

use crate::Duty;
use serde::{Deserialize, Serialize};

/// What the actuators should be set to at one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Setpoint {
    pub light: Duty,
    pub pump: Duty,
}

impl Setpoint {
    /// Everything off. What a schedule with no light hours produces at night.
    pub const DARK: Setpoint = Setpoint {
        light: Duty::OFF,
        pump: Duty::OFF,
    };
}

/// A day's light and pump programme.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    /// Local hour the lights start ramping up, 0-23.
    pub light_start_hour: u8,
    /// Hours of light per day, including both ramps.
    pub light_hours: f32,
    /// Duty at full daylight.
    pub light_duty: f32,
    /// Minutes spent ramping up at dawn and down at dusk.
    ///
    /// The stock firmware ramps rather than switching, and `garden-edge watch-pwm`
    /// records the real curve during Phase 1. Replicating it is the whole point of
    /// parity capture: plants that have adapted to a gradual dawn respond badly to
    /// being switched on at full output, and a step change is also a current spike the
    /// supply has to absorb every morning.
    pub ramp_minutes: f32,
    /// Minutes the pump runs at the start of each cycle.
    pub pump_on_minutes: f32,
    /// Length of one pump cycle.
    pub pump_cycle_minutes: f32,
    /// Duty while the pump runs. On this hardware the pump is a switch, so anything
    /// non-zero means on — see `Duty::PUMP_MAX` for why the field survives anyway.
    pub pump_duty: f32,
}

impl Schedule {
    /// A reasonable general-purpose programme.
    ///
    /// Sixteen hours of light suits the leafy greens that make up most of Gardyn's
    /// catalogue. The pump runs through the dark hours too — roots do not stop needing
    /// water when the lights go off, and a schedule that tied them together would dry
    /// the tower out overnight.
    ///
    /// **Pump timing is the factory's, by decision on 2026-08-30**: four five-minute
    /// runs a day. This used to be fifteen minutes in every hour — six hours a day
    /// against the factory's twenty minutes — which was a guess made before anyone had
    /// read the stock schedule. See `baseline/factory-schedule.md`. The lighting is
    /// still our own: a longer photoperiod at lower output than the factory's
    /// thirteen-hour block at 100%.
    pub const DEFAULT: Schedule = Schedule {
        light_start_hour: 6,
        light_hours: 16.0,
        light_duty: 0.85,
        ramp_minutes: 30.0,
        pump_on_minutes: 5.0,
        pump_cycle_minutes: 360.0,
        // On. Binary hardware, so any non-zero value means the same thing; 1.0 says so.
        pump_duty: 1.0,
    };

    /// The programme the failsafe runs: **the factory's own**, as far as this model
    /// can express it. See `baseline/factory-schedule.md`.
    ///
    /// This used to be a set of defensible guesses — 14 hours from midnight, pump 15
    /// minutes in every hour. Phase 0 read the stock schedule off the device, and the
    /// guesses were not close. The factory pumps for **20 minutes a day**, in four
    /// five-minute runs; the old failsafe pumped for six hours. Whichever of those is
    /// better for the plants, only one of them is a programme these particular plants
    /// are known to have survived, and that is the one a failsafe should run.
    ///
    /// Light 07:00–22:00, an hour ramping at each end against the factory's hour at
    /// half output — not identical, but the same shape and a similar daily total. Pump
    /// every six hours rather than at the factory's four uneven times, because a
    /// cycle length is all this model holds and four runs a day is the part that
    /// matters.
    pub const FAILSAFE: Schedule = Schedule {
        light_start_hour: 7,
        light_hours: 15.0,
        light_duty: 1.0,
        ramp_minutes: 60.0,
        pump_on_minutes: 5.0,
        pump_cycle_minutes: 360.0,
        // Full on, which is what the factory does. The 30% ceiling this was written
        // against has since been retired — see `Duty::PUMP_MAX`.
        pump_duty: 1.0,
    };

    /// What to drive at `seconds_since_midnight`, local time.
    pub fn setpoint(&self, seconds_since_midnight: u32) -> Setpoint {
        Setpoint {
            light: Duty::new(self.light_level(seconds_since_midnight)),
            pump: if self.pump_running(seconds_since_midnight) {
                // Still through `Duty::pump`, which clamps whatever the schedule asks
                // for. The ceiling is now 1.0 rather than 0.30, so this bounds nonsense
                // rather than current — but a schedule arriving over the network is
                // exactly the thing that should not be trusted to be sane.
                Duty::pump(self.pump_duty)
            } else {
                Duty::OFF
            },
        }
    }

    /// Light level in `0.0..=1.0`, including the dawn and dusk ramps.
    fn light_level(&self, seconds_since_midnight: u32) -> f32 {
        let hours = self.light_hours.clamp(0.0, 24.0);
        if hours <= 0.0 {
            return 0.0;
        }
        let peak = self.light_duty.clamp(0.0, 1.0);

        // Elapsed hours since the lights started, wrapping at midnight so a programme
        // that begins at 18:00 still works.
        let now_h = seconds_since_midnight as f32 / 3600.0;
        let start = f32::from(self.light_start_hour.min(23));
        let elapsed = (now_h - start).rem_euclid(24.0);

        if elapsed >= hours {
            return 0.0;
        }

        // A ramp longer than half the photoperiod would overlap itself, which would
        // make the lights dimmest in the middle of the day.
        let ramp_h = (self.ramp_minutes.max(0.0) / 60.0).min(hours / 2.0);
        if ramp_h <= 0.0 {
            return peak;
        }

        if elapsed < ramp_h {
            peak * (elapsed / ramp_h)
        } else if elapsed > hours - ramp_h {
            peak * ((hours - elapsed) / ramp_h)
        } else {
            peak
        }
    }

    fn pump_running(&self, seconds_since_midnight: u32) -> bool {
        let cycle = self.pump_cycle_minutes;
        if cycle <= 0.0 {
            return false;
        }
        let on = self.pump_on_minutes.clamp(0.0, cycle);
        if on <= 0.0 {
            return false;
        }
        // Anchored to midnight rather than to boot, so a Pi that reboots resumes the
        // same cycle instead of restarting it and over-watering.
        let minute_in_cycle = (seconds_since_midnight as f32 / 60.0) % cycle;
        minute_in_cycle < on
    }

    /// Total light energy per day, as duty-hours.
    ///
    /// The number to compare when changing a schedule: dropping two hours and raising
    /// the duty can leave the plants with the same daily light integral, and comparing
    /// hours alone would hide that.
    pub fn daily_duty_hours(&self) -> f32 {
        let hours = self.light_hours.clamp(0.0, 24.0);
        let peak = self.light_duty.clamp(0.0, 1.0);
        let ramp_h = (self.ramp_minutes.max(0.0) / 60.0).min(hours / 2.0);
        // Two triangular ramps plus the flat middle.
        (hours - ramp_h) * peak
    }

    /// Whether this schedule is physically sane.
    ///
    /// Checked on arrival from the network. A schedule is the one thing the brain can
    /// send that changes what the hardware does, so it is the one thing worth
    /// validating rather than clamping silently.
    pub fn validate(&self) -> Result<(), ScheduleError> {
        if self.light_start_hour > 23 {
            return Err(ScheduleError::StartHour(self.light_start_hour));
        }
        if !(0.0..=24.0).contains(&self.light_hours) || !self.light_hours.is_finite() {
            return Err(ScheduleError::LightHours(self.light_hours));
        }
        if !(0.0..=1.0).contains(&self.light_duty) || !self.light_duty.is_finite() {
            return Err(ScheduleError::LightDuty(self.light_duty));
        }
        if self.pump_cycle_minutes <= 0.0 || !self.pump_cycle_minutes.is_finite() {
            return Err(ScheduleError::PumpCycle(self.pump_cycle_minutes));
        }
        if self.pump_on_minutes < 0.0 || self.pump_on_minutes > self.pump_cycle_minutes {
            return Err(ScheduleError::PumpOn {
                on: self.pump_on_minutes,
                cycle: self.pump_cycle_minutes,
            });
        }
        if !(0.0..=Duty::PUMP_MAX).contains(&self.pump_duty) || !self.pump_duty.is_finite() {
            return Err(ScheduleError::PumpDuty(self.pump_duty));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ScheduleError {
    #[error("light start hour {0} is not an hour of the day")]
    StartHour(u8),
    #[error("light hours {0} is not between 0 and 24")]
    LightHours(f32),
    #[error("light duty {0} is not between 0 and 1")]
    LightDuty(f32),
    #[error("pump cycle {0} minutes is not a positive duration")]
    PumpCycle(f32),
    #[error("pump runs {on} minutes of a {cycle} minute cycle")]
    PumpOn { on: f32, cycle: f32 },
    #[error("pump duty {0} exceeds the {max} ceiling the supply can take", max = Duty::PUMP_MAX)]
    PumpDuty(f32),
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u32 = 3600;
    const MINUTE: u32 = 60;

    fn at(schedule: &Schedule, hour: f32) -> Setpoint {
        schedule.setpoint((hour * 3600.0) as u32)
    }

    #[test]
    fn the_default_schedule_is_valid_and_so_is_the_failsafe() {
        assert_eq!(Schedule::DEFAULT.validate(), Ok(()));
        assert_eq!(Schedule::FAILSAFE.validate(), Ok(()));
    }

    #[test]
    fn lights_follow_the_photoperiod() {
        let s = Schedule::DEFAULT; // 06:00, 16 hours, so dark from 22:00.
        assert!(at(&s, 3.0).light.is_off(), "before dawn");
        assert!(!at(&s, 12.0).light.is_off(), "midday");
        assert!(at(&s, 23.0).light.is_off(), "after dusk");
        assert!(at(&s, 5.9).light.is_off(), "a minute before start");
    }

    #[test]
    fn dawn_ramps_up_and_dusk_ramps_down() {
        // The behaviour parity capture exists to replicate. A step change is a current
        // spike every morning and a shock to plants adapted to a gradual dawn.
        let s = Schedule::DEFAULT;
        let dawn_start = at(&s, 6.0).light.get();
        let dawn_mid = at(&s, 6.25).light.get();
        let full = at(&s, 12.0).light.get();
        let dusk_mid = at(&s, 21.75).light.get();

        assert!(dawn_start < dawn_mid, "{dawn_start} then {dawn_mid}");
        assert!(dawn_mid < full, "{dawn_mid} then {full}");
        assert!((dawn_mid - full / 2.0).abs() < 0.05, "half way up at half a ramp");
        assert!(dusk_mid < full && dusk_mid > 0.0, "dusk: {dusk_mid}");
    }

    #[test]
    fn a_schedule_that_starts_in_the_evening_wraps_past_midnight() {
        let s = Schedule {
            light_start_hour: 18,
            light_hours: 12.0,
            ramp_minutes: 0.0,
            ..Schedule::DEFAULT
        };
        assert!(!at(&s, 19.0).light.is_off(), "evening");
        assert!(!at(&s, 2.0).light.is_off(), "still on after midnight");
        assert!(at(&s, 7.0).light.is_off(), "off in the morning");
    }

    #[test]
    fn an_absurdly_long_ramp_cannot_dim_the_middle_of_the_day() {
        // A ramp longer than half the photoperiod would overlap itself and make noon
        // the darkest part of the day.
        let s = Schedule {
            ramp_minutes: 10_000.0,
            ..Schedule::DEFAULT
        };
        let noon = at(&s, 14.0).light.get();
        for hour in [7.0, 10.0, 18.0, 21.0] {
            assert!(at(&s, hour).light.get() <= noon + 1e-6, "dimmer at {hour}");
        }
    }

    #[test]
    fn the_pump_cycles_and_keeps_going_through_the_night() {
        let s = Schedule::DEFAULT; // 5 minutes in every 360, as the factory runs it.
        assert!(!s.setpoint(0).pump.is_off());
        assert!(!s.setpoint(4 * MINUTE).pump.is_off());
        assert!(s.setpoint(6 * MINUTE).pump.is_off());
        assert!(s.setpoint(5 * HOUR).pump.is_off());
        assert!(!s.setpoint(6 * HOUR).pump.is_off(), "next cycle");

        // Roots do not stop needing water when the lights go off. Both the 00:00 and
        // the 18:00 cycle fall outside the 06:00–22:00 photoperiod.
        let night = s.setpoint(0);
        assert!(night.light.is_off());
        assert!(!night.pump.is_off());
    }

    #[test]
    fn the_default_and_the_failsafe_pump_for_as_long_as_the_factory_does() {
        // Twenty minutes a day, per baseline/factory-schedule.md. Both of these used to
        // run the pump for six hours a day on a figure nobody had checked, so the
        // assertion is on the daily total rather than on the cycle constants.
        for schedule in [Schedule::DEFAULT, Schedule::FAILSAFE] {
            let minutes = (0..86_400)
                .step_by(60)
                .filter(|s| !schedule.setpoint(*s).pump.is_off())
                .count();
            assert_eq!(minutes, 20, "{minutes} minutes a day for {schedule:?}");
        }
    }

    #[test]
    fn the_pump_cycle_is_anchored_to_midnight_not_to_boot() {
        // A Pi that reboots mid-cycle must resume where the clock says, not restart
        // the cycle and water twice.
        let s = Schedule::DEFAULT;
        assert_eq!(s.setpoint(70 * MINUTE), s.setpoint(130 * MINUTE));
    }

    #[test]
    fn a_schedule_can_never_drive_the_pump_past_its_ceiling() {
        // Full-on is now legitimate — it is what the factory does four times a day —
        // so the schedule that used to be the "greedy" one is simply the correct one.
        let full = Schedule {
            pump_duty: 1.0,
            ..Schedule::DEFAULT
        };
        assert_eq!(full.validate(), Ok(()));

        // Nonsense is still refused, at validation and again in the type, because a
        // schedule arriving over the network is not to be trusted with either.
        let absurd = Schedule {
            pump_duty: 4.0,
            ..Schedule::DEFAULT
        };
        assert_eq!(absurd.validate(), Err(ScheduleError::PumpDuty(4.0)));
        for second in (0..86_400).step_by(300) {
            assert!(absurd.setpoint(second).pump.get() <= Duty::PUMP_MAX);
        }
    }

    #[test]
    fn nothing_yet_bounds_how_long_a_schedule_may_run_the_pump() {
        // Recorded rather than asserted-away, because it is the gap left by retiring the
        // duty ceiling. The factory's guardian forces the pump off after fifteen
        // minutes; `validate` will happily accept a schedule that runs it for six hours,
        // and on binary hardware that is six hours of pumping rather than six hours at
        // 30%. Phase 6 owes the actuator layer that guard — see `Duty::PUMP_MAX`.
        let all_day = Schedule {
            pump_on_minutes: 360.0,
            pump_cycle_minutes: 360.0,
            ..Schedule::DEFAULT
        };
        assert_eq!(all_day.validate(), Ok(()), "still accepted today");
        assert!(!all_day.setpoint(3 * HOUR).pump.is_off());
    }

    #[test]
    fn nonsense_schedules_are_refused_rather_than_clamped() {
        let cases = [
            Schedule {
                light_start_hour: 25,
                ..Schedule::DEFAULT
            },
            Schedule {
                light_hours: f32::NAN,
                ..Schedule::DEFAULT
            },
            Schedule {
                light_duty: 4.0,
                ..Schedule::DEFAULT
            },
            Schedule {
                pump_cycle_minutes: 0.0,
                ..Schedule::DEFAULT
            },
            Schedule {
                pump_on_minutes: 90.0,
                pump_cycle_minutes: 60.0,
                ..Schedule::DEFAULT
            },
        ];
        for schedule in cases {
            assert!(schedule.validate().is_err(), "{schedule:?} should be refused");
        }
    }

    #[test]
    fn a_dark_schedule_produces_darkness_rather_than_dividing_by_zero() {
        let dark = Schedule {
            light_hours: 0.0,
            pump_on_minutes: 0.0,
            ..Schedule::DEFAULT
        };
        for second in (0..86_400).step_by(600) {
            assert_eq!(dark.setpoint(second), Setpoint::DARK);
        }
    }

    #[test]
    fn daily_light_is_comparable_across_reshaped_schedules() {
        // The number to look at when changing a programme: fewer hours at a higher
        // duty can deliver the same daily light, and comparing hours would hide it.
        let long_dim = Schedule {
            light_hours: 18.0,
            light_duty: 0.6,
            ramp_minutes: 0.0,
            ..Schedule::DEFAULT
        };
        let short_bright = Schedule {
            light_hours: 12.0,
            light_duty: 0.9,
            ramp_minutes: 0.0,
            ..Schedule::DEFAULT
        };
        assert!((long_dim.daily_duty_hours() - short_bright.daily_duty_hours()).abs() < 0.01);
    }

    #[test]
    fn a_ramp_reduces_the_daily_light_it_replaces() {
        let sharp = Schedule {
            ramp_minutes: 0.0,
            ..Schedule::DEFAULT
        };
        assert!(Schedule::DEFAULT.daily_duty_hours() < sharp.daily_duty_hours());
    }

    #[test]
    fn a_schedule_round_trips_through_json() {
        let text = serde_json::to_string(&Schedule::DEFAULT).unwrap();
        assert_eq!(
            serde_json::from_str::<Schedule>(&text).unwrap(),
            Schedule::DEFAULT
        );
    }
}
