//! Hardware abstraction for the Garden edge agent.
//!
//! Everything the edge agent touches is behind a trait here, for two reasons. The
//! obvious one is testability. The load-bearing one is that it lets the entire brain,
//! rule engine, and vision pipeline be developed against `garden-sim` on a desktop,
//! with no Pi in the loop — most of the work in this project is not hardware work and
//! should not be gated on hardware.

pub mod handover;
pub mod schedule;

pub use handover::{GuardMarker, Heartbeat};
pub use schedule::{Schedule, ScheduleError, Setpoint};

use garden_core::{CapabilitySet, SensorSnapshot};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum HalError {
    #[error("sensor {sensor} unavailable: {reason}")]
    SensorUnavailable {
        sensor: &'static str,
        reason: String,
    },
    #[error("bus error on {bus}: {reason}")]
    Bus { bus: &'static str, reason: String },
    #[error("actuator refused: {0}")]
    Actuator(String),
    #[error("camera error: {0}")]
    Camera(String),
}

pub type Result<T> = std::result::Result<T, HalError>;

/// Injectable time source, so simulations can run a season in a millisecond and tests
/// are not wall-clock dependent.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// Real time.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// A PWM duty cycle in `0.0..=1.0`.
///
/// Constructing one always clamps, so an out-of-range value is impossible to hold
/// rather than merely discouraged.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Duty(f32);

impl Duty {
    pub const OFF: Duty = Duty(0.0);
    pub const FULL: Duty = Duty(1.0);

    /// Hard ceiling on pump duty.
    ///
    /// Full-on is believed to exceed the supply's current budget, so the cap is
    /// enforced in the type rather than left to calling code to remember. This is the
    /// single most consequential invariant in the edge agent: exceeding it risks the
    /// power supply, and after firmware takeover there is no vendor firmware left to
    /// catch the mistake.
    pub const PUMP_MAX: f32 = 0.30;

    /// A general duty, clamped to the valid range.
    pub fn new(v: f32) -> Self {
        Duty(if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) })
    }

    /// A pump duty, additionally clamped to [`Duty::PUMP_MAX`].
    pub fn pump(v: f32) -> Self {
        Duty(if v.is_nan() {
            0.0
        } else {
            v.clamp(0.0, Self::PUMP_MAX)
        })
    }

    pub fn get(self) -> f32 {
        self.0
    }

    pub fn is_off(self) -> bool {
        self.0 <= f32::EPSILON
    }

    pub fn percent(self) -> f32 {
        self.0 * 100.0
    }

    /// Duty in thousandths, which is how a frame records the light it was shot under.
    pub fn milli(self) -> u16 {
        (self.0 * 1000.0).round().clamp(0.0, 1000.0) as u16
    }

    /// A duty from a whole percentage, usable in a `const`.
    ///
    /// `Duty::new` clamps at runtime and cannot be const because of the NaN check;
    /// this takes an integer percentage, which has no NaN to worry about.
    pub const fn from_percent_const(percent: u8) -> Self {
        let clamped = if percent > 100 { 100 } else { percent };
        Duty(clamped as f32 / 100.0)
    }
}

/// Reads every fitted sensor in one pass.
pub trait SensorBank: Send {
    /// Which sensing capabilities this bank currently provides. Derived from what
    /// actually reads back, so a failed probe degrades the system automatically.
    fn capabilities(&self) -> CapabilitySet;

    fn read(&mut self) -> Result<SensorSnapshot>;
}

/// Light and pump control. Only available once firmware takeover has happened.
pub trait Actuators: Send {
    fn set_light(&mut self, duty: Duty) -> Result<()>;
    fn set_pump(&mut self, duty: Duty) -> Result<()>;
    fn light(&self) -> Duty;
    fn pump(&self) -> Duty;
}

/// A captured frame. Pixel format is deliberately opaque here; `garden-vision` owns
/// decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub captured_at: Timestamp,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    /// Light duty at capture time. Recorded because photometric comparability across
    /// frames depends on it — see `photo_mode` below.
    pub light_duty_milli: u16,
}

pub trait Camera: Send {
    fn capture(&mut self) -> Result<Frame>;
}

/// Captures a frame under a known, fixed light level.
///
/// This is the concrete payoff of owning the firmware. Under the stock sunrise/sunset
/// ramp, brightness varies with capture time, so colour-derived measurements such as
/// the chlorosis index are not comparable between frames. Pinning the light to a
/// reference duty for the duration of the capture makes every frame photometrically
/// comparable, which is what turns canopy colour into a usable signal rather than an
/// artefact of when the photo was taken.
pub fn photo_mode<A: Actuators, C: Camera>(
    actuators: &mut A,
    camera: &mut C,
    reference: Duty,
    settle: impl FnOnce(),
) -> Result<Frame> {
    let restore = actuators.light();
    actuators.set_light(reference)?;
    settle();
    let frame = camera.capture();
    // Restore the operating light level even if the capture failed.
    actuators.set_light(restore)?;

    // Stamp the level actually pinned. This function is the only code that knows it
    // for certain — a camera reports pixels, not what the room was lit at — and
    // leaving the caller to remember is how a frame ends up labelled with the wrong
    // brightness and silently poisons a colour trend.
    frame.map(|mut frame| {
        frame.light_duty_milli = reference.milli();
        frame
    })
}

/// The level every pinned capture is taken at.
///
/// One constant, and the same every time, which is the entire point: comparing the
/// colour of two frames only means anything if they were lit identically. Chosen to
/// match the failsafe's daylight level so a photograph does not visibly change the
/// room.
pub const PHOTO_REFERENCE: Duty = Duty::from_percent_const(80);

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn duty_clamps_out_of_range_values() {
        assert_eq!(Duty::new(-1.0).get(), 0.0);
        assert_eq!(Duty::new(2.0).get(), 1.0);
        assert_eq!(Duty::new(0.5).get(), 0.5);
    }

    #[test]
    fn nan_duty_fails_safe_to_off() {
        assert_eq!(Duty::new(f32::NAN).get(), 0.0);
        assert_eq!(Duty::pump(f32::NAN).get(), 0.0);
    }

    #[test]
    fn pump_duty_cannot_exceed_the_current_budget() {
        assert_eq!(Duty::pump(1.0).get(), Duty::PUMP_MAX);
        assert_eq!(Duty::pump(0.9).get(), Duty::PUMP_MAX);
        assert_eq!(Duty::pump(0.2).get(), 0.2);
    }

    struct FakeActuators {
        light: Duty,
        pump: Duty,
    }

    impl Actuators for FakeActuators {
        fn set_light(&mut self, duty: Duty) -> Result<()> {
            self.light = duty;
            Ok(())
        }
        fn set_pump(&mut self, duty: Duty) -> Result<()> {
            self.pump = duty;
            Ok(())
        }
        fn light(&self) -> Duty {
            self.light
        }
        fn pump(&self) -> Duty {
            self.pump
        }
    }

    struct FakeCamera {
        light_at_capture: Cell<f32>,
        fail: bool,
    }

    impl Camera for FakeCamera {
        fn capture(&mut self) -> Result<Frame> {
            if self.fail {
                return Err(HalError::Camera("sensor timeout".into()));
            }
            Ok(Frame {
                captured_at: Timestamp::from_second(0).unwrap(),
                width: 4,
                height: 4,
                data: vec![0; 16],
                light_duty_milli: (self.light_at_capture.get() * 1000.0) as u16,
            })
        }
    }

    #[test]
    fn photo_mode_pins_then_restores_the_light() {
        let mut act = FakeActuators {
            light: Duty::new(0.62),
            pump: Duty::pump(0.2),
        };
        let observed = Cell::new(0.0);
        let mut cam = FakeCamera {
            light_at_capture: Cell::new(0.0),
            fail: false,
        };

        let reference = Duty::new(0.80);
        let frame = {
            let light_probe = &observed;
            photo_mode(&mut act, &mut cam, reference, || {
                light_probe.set(0.80);
            })
        }
        .unwrap();

        // Capture happened at the reference level...
        assert_eq!(observed.get(), 0.80);
        assert_eq!(frame.width, 4);
        // ...and the operating level was put back.
        assert_eq!(act.light(), Duty::new(0.62));
    }

    #[test]
    fn photo_mode_restores_the_light_even_when_capture_fails() {
        let mut act = FakeActuators {
            light: Duty::new(0.62),
            pump: Duty::pump(0.2),
        };
        let mut cam = FakeCamera {
            light_at_capture: Cell::new(0.0),
            fail: true,
        };

        let result = photo_mode(&mut act, &mut cam, Duty::new(0.8), || {});
        assert!(result.is_err());
        // A failed capture must not strand the garden at the reference brightness.
        assert_eq!(act.light(), Duty::new(0.62));
    }
}

#[cfg(test)]
mod photo_tests {
    use super::*;
    use std::cell::Cell;

    struct Act {
        light: Duty,
        refused: bool,
    }

    impl Actuators for Act {
        fn set_light(&mut self, duty: Duty) -> Result<()> {
            if self.refused {
                return Err(HalError::Actuator("the failsafe owns the pins".into()));
            }
            self.light = duty;
            Ok(())
        }
        fn set_pump(&mut self, _: Duty) -> Result<()> {
            Ok(())
        }
        fn light(&self) -> Duty {
            self.light
        }
        fn pump(&self) -> Duty {
            Duty::OFF
        }
    }

    struct Cam {
        seen: Cell<u16>,
    }

    impl Camera for Cam {
        fn capture(&mut self) -> Result<Frame> {
            Ok(Frame {
                captured_at: Timestamp::from_second(0).unwrap(),
                width: 4,
                height: 4,
                data: vec![0; 16],
                // Deliberately wrong, to prove `photo_mode` overwrites it: a camera
                // reports pixels, not what the room was lit at.
                light_duty_milli: self.seen.get(),
            })
        }
    }

    #[test]
    fn the_frame_records_the_level_it_was_actually_pinned_at() {
        let mut act = Act {
            light: Duty::new(0.62),
            refused: false,
        };
        let mut cam = Cam { seen: Cell::new(1) };

        let frame = photo_mode(&mut act, &mut cam, PHOTO_REFERENCE, || {}).unwrap();
        assert_eq!(frame.light_duty_milli, 800, "the reference, not the camera's guess");
        assert_eq!(act.light(), Duty::new(0.62), "and the operating level is back");
    }

    #[test]
    fn a_refused_pin_fails_rather_than_taking_an_unlit_photograph() {
        // If the failsafe has the pins, we cannot guarantee the light level, so the
        // frame must not be labelled comparable. Failing lets the caller fall back to
        // an honest ambient capture.
        let mut act = Act {
            light: Duty::new(0.62),
            refused: true,
        };
        let mut cam = Cam { seen: Cell::new(0) };
        assert!(photo_mode(&mut act, &mut cam, PHOTO_REFERENCE, || {}).is_err());
    }

    #[test]
    fn the_reference_is_a_round_number_and_inside_range() {
        assert_eq!(PHOTO_REFERENCE.milli(), 800);
        assert!(PHOTO_REFERENCE.get() > 0.0 && PHOTO_REFERENCE.get() <= 1.0);
    }

    #[test]
    fn milli_rounds_rather_than_truncates() {
        // 0.2999 is 300 thousandths, not 299. Truncating would make two identical
        // captures differ by one unit and look like a light change.
        assert_eq!(Duty::new(0.2999).milli(), 300);
        assert_eq!(Duty::new(0.0).milli(), 0);
        assert_eq!(Duty::FULL.milli(), 1000);
    }
}
