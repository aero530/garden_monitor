//! The guard's own pin driver.
//!
//! Deliberately a second, independent implementation rather than a shared one with
//! `garden-edge`. The whole reason this is a separate process is that a fault in the
//! complicated program must not be able to take out the simple one, and sharing a
//! driver would put the agent's code back inside the failsafe's address space.
//!
//! It is also much smaller, because it needs less: no photo mode, no unchanged-value
//! tracking, no error recovery beyond logging. Set two pins, or say why not.

use garden_hal::{Duty, PumpGuard, Setpoint};

#[derive(Debug, thiserror::Error)]
pub enum PinError {
    #[allow(dead_code)]
    #[error("GPIO unavailable: {0}")]
    Gpio(String),
    #[allow(dead_code)]
    #[error("this build has no actuator support; it was compiled for {0}")]
    Unsupported(&'static str),
}

/// Light on GPIO18 (hardware PWM channel 0), pump on GPIO24 (software PWM).
///
/// Restated here rather than shared with the agent, for the same reason the driver is:
/// the failsafe should not need the agent's crates to be correct in order to work.
#[cfg_attr(
    not(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64"))),
    allow(dead_code)
)]
const GPIO_LIGHT: u8 = 18;
#[cfg_attr(
    not(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64"))),
    allow(dead_code)
)]
const GPIO_PUMP: u8 = 24;

#[cfg(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64")))]
mod imp {
    use super::*;
    use rppal::gpio::{Gpio, OutputPin};
    use rppal::pwm::{Channel, Polarity, Pwm};

    /// Measured on the device, 2026-08-30: `pfg 18` reports 20 kHz, not the 8 kHz the
    /// Home 3.0/4.0 community map gives. The failsafe exists to look like the factory
    /// firmware when the agent is gone, so it uses the frequency the factory actually
    /// uses. LED drivers are not always indifferent to this — some flicker or buzz at
    /// the wrong rate — and it costs nothing to match.
    const LIGHT_HZ: f64 = 20_000.0;

    pub struct Pins {
        light: Pwm,
        pump: OutputPin,
    }

    impl Pins {
        pub fn open() -> Result<Self, PinError> {
            let light = Pwm::with_frequency(Channel::Pwm0, LIGHT_HZ, 0.0, Polarity::Normal, true)
                .map_err(|e| PinError::Gpio(format!("hardware PWM on GPIO{GPIO_LIGHT}: {e}")))?;
            let pump = Gpio::new()
                .map_err(|e| PinError::Gpio(e.to_string()))?
                .get(GPIO_PUMP)
                .map_err(|e| PinError::Gpio(format!("GPIO{GPIO_PUMP}: {e}")))?
                .into_output_low();
            Ok(Self { light, pump })
        }

        fn set_light(&mut self, duty: Duty) -> Result<(), PinError> {
            self.light
                .set_duty_cycle(f64::from(duty.get()))
                .map_err(|e| PinError::Gpio(format!("light: {e}")))
        }

        /// The pump is a switch, not a dimmer.
        ///
        /// This used to drive software PWM at 500 Hz. Phase 0 showed the factory does
        /// nothing of the kind: `Pump.py` calls `set_mode(24, OUTPUT)` then
        /// `write(24, 1)` or `write(24, 0)`, and the overnight capture confirmed it —
        /// `gdc 24` answered `PI_NOT_PWM_GPIO` for thirteen hours, including through two
        /// five-minute runs with the pump audibly going.
        ///
        /// Matching that is a safety change, not a tidiness one. Nothing tells us what
        /// sits between GPIO24 and the motor. If it is a relay or a driver stage meant
        /// for a static level, switching it at 500 Hz is somewhere between "does not
        /// pump" and "destroys the relay" — and the failsafe is the last thing that
        /// should be improvising on unverified hardware.
        ///
        /// So any non-zero duty means on. See DESIGN.md §5.
        pub fn set_pump(&mut self, duty: Duty) -> Result<(), PinError> {
            if duty.is_off() {
                self.pump.set_low();
            } else {
                self.pump.set_high();
            }
            Ok(())
        }

        pub fn drive(&mut self, setpoint: Setpoint) -> Result<(), PinError> {
            self.set_light(setpoint.light)?;
            self.set_pump(setpoint.pump)
        }
    }
}

#[cfg(not(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64"))))]
mod imp {
    use super::*;

    /// Records the last setpoint, so the supervision loop and the handover can be
    /// exercised on a desktop.
    pub struct Pins {
        pub last: Option<Setpoint>,
    }

    impl Pins {
        pub fn open() -> Result<Self, PinError> {
            Ok(Self { last: None })
        }

        pub fn set_pump(&mut self, duty: Duty) -> Result<(), PinError> {
            let light = self.last.map(|s| s.light).unwrap_or(Duty::OFF);
            self.last = Some(Setpoint { light, pump: duty });
            Ok(())
        }

        pub fn drive(&mut self, setpoint: Setpoint) -> Result<(), PinError> {
            self.last = Some(setpoint);
            Ok(())
        }
    }
}

pub use imp::Pins;

/// A driver that may or may not exist, plus the dry-run decision.
///
/// `Dry` is the default until Phase 6 and is not a stub: it runs the whole supervision
/// loop, computes every setpoint, and logs what it would have driven. That is exactly
/// what you want for the weeks of watching that should precede trusting a watchdog
/// with a crop.
pub enum Output {
    Dry,
    Live(Pins),
}

/// The pump's on-time ceiling, held here as well as in the agent.
///
/// Deliberately not shared state with `garden-edge`: the whole premise of this process
/// is that it keeps working when the complicated one has failed, and a guard that
/// depended on the agent's bookkeeping would inherit whatever went wrong there. Two
/// independent fifteen-minute timers is the correct amount of duplication.
static PUMP_GUARD: std::sync::Mutex<PumpGuard> = std::sync::Mutex::new(PumpGuard::new());

/// Apply the on-time ceiling to a setpoint before it reaches a pin.
fn bounded(setpoint: Setpoint) -> Setpoint {
    let now = std::time::Instant::now();
    let Ok(mut guard) = PUMP_GUARD.lock() else {
        // A poisoned lock means a panic while holding it. The failsafe's job is to fail
        // safe, and for a pump that means off.
        tracing::error!("the pump guard is poisoned; refusing to run the pump");
        return Setpoint {
            pump: Duty::OFF,
            ..setpoint
        };
    };
    let allowed = guard.allow(setpoint.pump, now);
    if allowed.is_off() && !setpoint.pump.is_off() {
        tracing::error!(
            limit_minutes = garden_hal::PUMP_MAX_RUN.as_secs() / 60,
            "the failsafe schedule has been asking for the pump too long; forcing it off"
        );
    }
    Setpoint {
        pump: allowed,
        ..setpoint
    }
}

impl Output {
    pub fn open(dry_run: bool) -> Self {
        if dry_run {
            return Output::Dry;
        }
        match Pins::open() {
            Ok(pins) => Output::Live(pins),
            Err(e) => {
                // A failsafe that refuses to start because it could not open a pin is
                // worse than one that keeps watching and says so: the next attempt may
                // succeed, and until then the log is the only warning anyone gets.
                tracing::error!(%e, "cannot take the pins; continuing in dry-run");
                Output::Dry
            }
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, Output::Live(_))
    }

    pub fn drive(&mut self, setpoint: Setpoint) {
        // Bounded even in dry run, so the log shows what would actually have happened
        // rather than what the schedule asked for. A dry run that reported a six-hour
        // pump the live path would have cut is worse than no dry run.
        let setpoint = bounded(setpoint);
        match self {
            Output::Dry => tracing::info!(
                light = setpoint.light.percent(),
                pump = setpoint.pump.percent(),
                "would drive"
            ),
            Output::Live(pins) => {
                if let Err(e) = pins.drive(setpoint) {
                    tracing::error!(%e, "failsafe could not drive the pins");
                }
            }
        }
    }

    /// Stop the pump and leave the lights alone.
    ///
    /// Called when handing control back. The asymmetry is deliberate and matches the
    /// agent's own shutdown: a pump left running floods a room, while lights left on
    /// for a few extra seconds cost nothing — and switching them off on every handover
    /// would strobe the garden each time the agent restarted.
    pub fn release_pump(&mut self) {
        if let Output::Live(pins) = self
            && let Err(e) = pins.set_pump(Duty::OFF)
        {
            tracing::error!(%e, "could not stop the pump on stand-down");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use garden_hal::Schedule;

    #[test]
    fn dry_run_never_opens_a_pin() {
        assert!(!Output::open(true).is_live());
    }

    #[test]
    fn a_live_output_records_what_it_drove() {
        let mut output = Output::open(false);
        assert!(output.is_live(), "the desktop backend always opens");
        let setpoint = Schedule::FAILSAFE.setpoint(6 * 3600);
        output.drive(setpoint);
        match output {
            Output::Live(pins) => assert_eq!(pins.last, Some(setpoint)),
            Output::Dry => panic!("expected a live output"),
        }
    }

    #[test]
    fn dry_run_accepts_a_setpoint_without_touching_anything() {
        let mut output = Output::open(true);
        output.drive(Schedule::FAILSAFE.setpoint(0));
        assert!(!output.is_live());
    }

    #[test]
    fn standing_down_stops_the_pump_and_leaves_the_lights_alone() {
        // A pump left running floods a room. Lights left on for a few extra seconds
        // cost nothing, and switching them off on every handover would strobe the
        // garden each time the agent restarted.
        let mut output = Output::open(false);
        let daylight = Schedule::FAILSAFE.setpoint(6 * 3600);
        output.drive(daylight);
        assert!(!daylight.pump.is_off() || !daylight.light.is_off());

        output.release_pump();
        match output {
            Output::Live(pins) => {
                let last = pins.last.expect("something was driven");
                assert!(last.pump.is_off(), "the pump must stop");
                assert_eq!(last.light, daylight.light, "the lights must not change");
            }
            Output::Dry => panic!("expected a live output"),
        }
    }
}
