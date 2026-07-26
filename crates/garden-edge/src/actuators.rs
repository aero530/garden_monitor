//! Driving the light and pump pins. **Phase 6 only.**
//!
//! Everything else in this agent is read-only, and that is what makes phases 0 through
//! 5 safe: the factory firmware keeps the garden alive and the worst our bugs can do is
//! lose telemetry. This module is where that stops being true.
//!
//! Three things guard it, in order of how much they matter:
//!
//! 1. **It is off unless asked for.** `--own-actuators` is not a default and never
//!    will be. Without it, `Actuators::set_*` is not even constructed.
//! 2. **Every duty goes through [`Duty`]**, so the pump ceiling is enforced by a type
//!    rather than by remembering. After takeover there is no vendor firmware left to
//!    catch an over-current mistake.
//! 3. **Two writers are arbitrated by a marker file.** `garden-guard` seizes the pins
//!    when the agent stops beating; the agent stands down when it sees the marker. See
//!    [`GuardMarker`].
//!
//! GPIO18 has a hardware PWM channel. GPIO24 does not, so the pump runs on `rppal`'s
//! software PWM — a kernel-timed thread, which jitters by a millisecond or so under
//! load. That is invisible to a pump running fifteen minutes an hour, and would not be
//! acceptable for the lights.

use garden_hal::{Duty, GuardMarker, Setpoint};

/// One error type for both backends, so the run loop does not need to know which one
/// it was compiled against. Each variant is constructed by exactly one of them.
#[derive(Debug, thiserror::Error)]
pub enum ActuatorError {
    #[allow(dead_code)]
    #[error("GPIO unavailable: {0}")]
    Gpio(String),
    #[allow(dead_code)]
    #[error("this build has no actuator support; it was compiled for {0}")]
    Unsupported(&'static str),
}

/// Light and pump pins, taken from the recon contract rather than restated here so
/// that a Phase 0 correction reaches the driver too.
#[cfg_attr(
    not(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64"))),
    allow(unused_imports)
)]
use garden_proto::recon::expected::{GPIO_LIGHT, GPIO_PUMP};

// --- Real hardware -------------------------------------------------------------------

#[cfg(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64")))]
mod imp {
    use super::*;
    use rppal::gpio::{Gpio, OutputPin};
    use rppal::pwm::{Channel, Pwm};

    /// PWM carrier frequencies.
    ///
    /// 8 kHz for the lights: above the audible range, so no coil whine, and fast
    /// enough that a camera exposure integrates whole cycles instead of catching
    /// banding. The pump is a DC motor and only needs to beat its own mechanical
    /// response, so it runs slower — which keeps the software PWM thread's duty
    /// cycle honest under load.
    const LIGHT_HZ: f64 = 8_000.0;
    const PUMP_HZ: f64 = 500.0;

    pub struct PinDriver {
        /// Hardware PWM. GPIO18 is channel 0 on every Pi.
        light: Pwm,
        /// Software PWM: GPIO24 has no hardware channel.
        pump: OutputPin,
        light_duty: Duty,
        pump_duty: Duty,
    }

    impl PinDriver {
        pub fn open() -> Result<Self, ActuatorError> {
            let light = Pwm::with_frequency(Channel::Pwm0, LIGHT_HZ, 0.0, rppal::pwm::Polarity::Normal, true)
                .map_err(|e| ActuatorError::Gpio(format!("hardware PWM on GPIO{GPIO_LIGHT}: {e}")))?;

            let gpio = Gpio::new().map_err(|e| ActuatorError::Gpio(e.to_string()))?;
            let mut pump = gpio
                .get(GPIO_PUMP)
                .map_err(|e| ActuatorError::Gpio(format!("GPIO{GPIO_PUMP}: {e}")))?
                .into_output_low();
            // Start stopped, whatever the pin was doing before we took it.
            pump.set_low();

            Ok(Self {
                light,
                pump,
                light_duty: Duty::OFF,
                pump_duty: Duty::OFF,
            })
        }

        pub fn set_light(&mut self, duty: Duty) -> Result<(), ActuatorError> {
            self.light
                .set_duty_cycle(f64::from(duty.get()))
                .map_err(|e| ActuatorError::Gpio(format!("light duty: {e}")))?;
            self.light_duty = duty;
            Ok(())
        }

        pub fn set_pump(&mut self, duty: Duty) -> Result<(), ActuatorError> {
            if duty.is_off() {
                self.pump
                    .clear_pwm()
                    .map_err(|e| ActuatorError::Gpio(format!("pump stop: {e}")))?;
                self.pump.set_low();
            } else {
                self.pump
                    .set_pwm_frequency(PUMP_HZ, f64::from(duty.get()))
                    .map_err(|e| ActuatorError::Gpio(format!("pump duty: {e}")))?;
            }
            self.pump_duty = duty;
            Ok(())
        }

        pub fn light(&self) -> Duty {
            self.light_duty
        }

        pub fn pump(&self) -> Duty {
            self.pump_duty
        }
    }

    impl Drop for PinDriver {
        /// Leave the pins in a state a plant survives.
        ///
        /// The pump stops — a pump left running floods, and a stopped one is missed
        /// water. The lights are *left alone*: dropping this on a mid-afternoon restart
        /// should not plunge the garden into darkness for the rest of the day.
        fn drop(&mut self) {
            let _ = self.pump.clear_pwm();
            self.pump.set_low();
        }
    }
}

// --- Development machine --------------------------------------------------------------

#[cfg(not(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64"))))]
mod imp {
    use super::*;

    /// Records what would have been driven, so the run loop, the arbitration and the
    /// schedule can all be exercised on a desktop.
    pub struct PinDriver {
        light_duty: Duty,
        pump_duty: Duty,
    }

    impl PinDriver {
        pub fn open() -> Result<Self, ActuatorError> {
            Ok(Self {
                light_duty: Duty::OFF,
                pump_duty: Duty::OFF,
            })
        }

        pub fn set_light(&mut self, duty: Duty) -> Result<(), ActuatorError> {
            self.light_duty = duty;
            Ok(())
        }

        pub fn set_pump(&mut self, duty: Duty) -> Result<(), ActuatorError> {
            self.pump_duty = duty;
            Ok(())
        }

        pub fn light(&self) -> Duty {
            self.light_duty
        }

        pub fn pump(&self) -> Duty {
            self.pump_duty
        }
    }
}

pub use imp::PinDriver;

/// The agent's view of the pins: a driver, plus the arbitration that decides whether
/// it is allowed to use it.
pub struct OwnedActuators {
    driver: PinDriver,
    marker: GuardMarker,
    /// Whether we were standing down last tick, so the transition is logged once
    /// rather than every fifteen seconds.
    yielded: bool,
    applied: Option<Setpoint>,
}

impl OwnedActuators {
    pub fn open(marker: GuardMarker) -> Result<Self, ActuatorError> {
        Ok(Self {
            driver: PinDriver::open()?,
            marker,
            yielded: false,
            applied: None,
        })
    }

    /// Drive the pins toward `setpoint`, unless the failsafe has taken over.
    ///
    /// Returns whether anything was written. Unchanged setpoints are skipped: a PWM
    /// register does not need rewriting sixty times a minute with the same value, and
    /// skipping makes the log readable.
    pub fn apply(&mut self, setpoint: Setpoint) -> Result<Applied, ActuatorError> {
        if self.marker.engaged() {
            if !self.yielded {
                self.yielded = true;
                tracing::warn!(
                    marker = %self.marker.path().display(),
                    "the failsafe has taken the pins; standing down"
                );
            }
            // Forget what we last applied. The guard is driving now, so our idea of the
            // pin state is stale and the first write after handback must go through.
            self.applied = None;
            return Ok(Applied::YieldedToGuard);
        }
        if self.yielded {
            self.yielded = false;
            tracing::info!("the failsafe has stood down; resuming control");
        }

        if self.applied == Some(setpoint) {
            return Ok(Applied::Unchanged);
        }

        self.driver.set_light(setpoint.light)?;
        self.driver.set_pump(setpoint.pump)?;
        self.applied = Some(setpoint);
        Ok(Applied::Changed)
    }

    pub fn light(&self) -> Duty {
        self.driver.light()
    }

    pub fn pump(&self) -> Duty {
        self.driver.pump()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Changed,
    Unchanged,
    YieldedToGuard,
}

#[cfg(test)]
mod tests {
    use super::*;
    use garden_hal::Schedule;

    fn marker(name: &str) -> GuardMarker {
        // A unique-enough suffix without pulling `uuid` into the edge agent.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        GuardMarker::new(std::env::temp_dir().join(format!("garden-guard-{name}-{nanos}")))
    }

    #[test]
    fn a_fresh_driver_starts_with_everything_off() {
        let driver = PinDriver::open().unwrap();
        assert!(driver.light().is_off());
        assert!(driver.pump().is_off());
    }

    #[test]
    fn applying_a_setpoint_drives_both_pins() {
        let m = marker("apply");
        let mut actuators = OwnedActuators::open(m.clone()).unwrap();
        let setpoint = Schedule::DEFAULT.setpoint(12 * 3600);

        assert_eq!(actuators.apply(setpoint).unwrap(), Applied::Changed);
        assert_eq!(actuators.light(), setpoint.light);
        assert_eq!(actuators.pump(), setpoint.pump);
        let _ = m.release();
    }

    #[test]
    fn an_unchanged_setpoint_is_not_rewritten() {
        let m = marker("unchanged");
        let mut actuators = OwnedActuators::open(m.clone()).unwrap();
        let setpoint = Schedule::DEFAULT.setpoint(12 * 3600);

        assert_eq!(actuators.apply(setpoint).unwrap(), Applied::Changed);
        assert_eq!(actuators.apply(setpoint).unwrap(), Applied::Unchanged);
        let _ = m.release();
    }

    #[test]
    fn the_agent_stands_down_while_the_failsafe_holds_the_pins() {
        // The arbitration that keeps two writers off one pin.
        let m = marker("yield");
        let mut actuators = OwnedActuators::open(m.clone()).unwrap();
        let day = Schedule::DEFAULT.setpoint(12 * 3600);
        assert_eq!(actuators.apply(day).unwrap(), Applied::Changed);

        m.engage().unwrap();
        assert_eq!(actuators.apply(day).unwrap(), Applied::YieldedToGuard);

        m.release().unwrap();
        // ...and the first write after handback goes through even though the setpoint
        // has not changed, because the guard moved the pins while we were not looking.
        assert_eq!(actuators.apply(day).unwrap(), Applied::Changed);
    }

    #[test]
    fn a_marker_can_be_released_twice_without_erroring() {
        // The guard releases on stand-down and again on shutdown; a missing file is
        // the desired state, not a failure.
        let m = marker("release");
        m.engage().unwrap();
        assert!(m.engaged());
        m.release().unwrap();
        assert!(!m.engaged());
        m.release().unwrap();
    }

    #[test]
    fn the_pump_ceiling_survives_the_whole_pipeline() {
        // From a schedule that asks for full output, through the driver, to the pin.
        let m = marker("ceiling");
        let mut actuators = OwnedActuators::open(m.clone()).unwrap();
        let greedy = Schedule {
            pump_duty: 1.0,
            ..Schedule::DEFAULT
        };
        actuators.apply(greedy.setpoint(0)).unwrap();
        assert!(actuators.pump().get() <= Duty::PUMP_MAX);
        let _ = m.release();
    }
}
