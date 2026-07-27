//! Reading the tank level with the ultrasonic sensor.
//!
//! This is the sensor the whole water story rests on. Without it `water_level_mm` is
//! always absent, `Capability::WaterLevel` never appears, and `water-level` — the rule
//! behind "tell me when to add water" — can never fire. Everything downstream of it
//! already exists: `TankGeometry::volume_from_distance`, the consumption fit, the
//! forecast. This module is the missing input.
//!
//! **The timing is kernel-side, and that is what makes it work.** The sensor answers by
//! holding an echo pin high for as long as the sound took to return, and at 0.343 mm/µs
//! a millisecond of scheduling jitter would be 170 mm of error — the whole tank. So the
//! pulse is not measured by watching the pin from userspace. `rppal`'s interrupt
//! `Event::timestamp` comes from the gpiod v2 `timestamp_ns` field, taken by the kernel
//! at interrupt time; two of those subtract to a pulse width accurate to a few
//! microseconds, which is under 2 mm.
//!
//! Everything except the two blocking waits is a pure function, so the arithmetic is
//! tested on a desktop and only the edges themselves need a Pi.
//!
//! Which is also why the whole module is dead code off-target: on a desktop the only
//! caller of this arithmetic is its own test suite, because the backend there refuses
//! to open and returns no reading. Annotating once here beats eight scattered
//! `allow`s, and the alternative — a desktop backend that invents a plausible water
//! level — would drive a real "add water" notification from a fabricated number.
#![cfg_attr(
    not(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64"))),
    allow(dead_code)
)]

use std::time::Duration;

/// Speed of sound in dry air at 0 °C, m/s.
const SOUND_BASE_MPS: f32 = 331.3;
/// How much it gains per degree, m/s/°C.
const SOUND_PER_DEGREE: f32 = 0.606;
/// Assumed air temperature when the AM2320 is not reporting.
///
/// Ignoring temperature entirely would be a systematic error, not noise: cold air is
/// slow, so a cold room reads the water as further away — a tank that looks emptier
/// than it is, all winter.
const ASSUMED_TEMP_C: f32 = 20.0;

/// The sensor's usable range. Readings outside it are echoes off the tank wall, the
/// sensor's own ring-down, or a missed edge — not water.
pub const MIN_RANGE_MM: f32 = 20.0;
pub const MAX_RANGE_MM: f32 = 2_000.0;

/// How many pulses to fire per reading.
///
/// A water surface is not a flat mirror: it ripples when the pump runs, and a single
/// ping can scatter. The median of five is cheap — about 300 ms once a minute — and
/// rejects the outliers that mean the sound went somewhere unhelpful.
pub const SAMPLES: usize = 5;

/// How many of those must land in range before the reading is worth having.
pub const MIN_VALID: usize = 3;

/// Longest a legitimate echo can take, with margin. Past this the pulse was lost.
pub const ECHO_TIMEOUT: Duration = Duration::from_millis(60);

/// Speed of sound at a given air temperature, in mm per microsecond.
pub fn speed_mm_per_us(air_temp_c: Option<f32>) -> f32 {
    let t = air_temp_c
        .filter(|t| t.is_finite() && (-20.0..=60.0).contains(t))
        .unwrap_or(ASSUMED_TEMP_C);
    // m/s to mm/µs is a factor of 1000 mm/m over 1_000_000 µs/s.
    (SOUND_BASE_MPS + SOUND_PER_DEGREE * t) / 1000.0
}

/// Convert one echo pulse into a distance, or reject it.
///
/// The sound travels to the surface and back, hence the halving.
pub fn distance_mm(echo: Duration, air_temp_c: Option<f32>) -> Option<f32> {
    let micros = echo.as_secs_f64() * 1e6;
    if !micros.is_finite() || micros <= 0.0 {
        return None;
    }
    let mm = (micros as f32) * speed_mm_per_us(air_temp_c) / 2.0;
    (mm.is_finite() && (MIN_RANGE_MM..=MAX_RANGE_MM).contains(&mm)).then_some(mm)
}

/// Combine a burst of readings into one.
///
/// Median rather than mean, because the failure mode is a wild outlier — a scattered
/// ping reading twice the tank's depth — and a mean would let one of those move the
/// answer by centimetres. A median ignores it entirely.
pub fn combine(samples: &[f32]) -> Option<f32> {
    let mut valid: Vec<f32> = samples
        .iter()
        .copied()
        .filter(|mm| mm.is_finite() && (MIN_RANGE_MM..=MAX_RANGE_MM).contains(mm))
        .collect();
    if valid.len() < MIN_VALID {
        return None;
    }
    valid.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    Some(valid[valid.len() / 2])
}

/// One error type for both backends, so the caller need not know which it compiled
/// against. Each variant is constructed by exactly one of them.
#[derive(Debug, thiserror::Error)]
pub enum UltrasonicError {
    #[allow(dead_code)]
    #[error("GPIO unavailable: {0}")]
    Gpio(String),
    #[allow(dead_code)]
    #[error("this build has no GPIO support; it was compiled for {0}")]
    Unsupported(&'static str),
}

// --- Real hardware -------------------------------------------------------------------

#[cfg(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64")))]
mod imp {
    use super::*;
    /// Trigger and echo pins, from the recon contract rather than restated here, so a
    /// Phase 0 correction reaches the driver too.
    use garden_proto::recon::expected::{GPIO_ULTRASONIC_ECHO, GPIO_ULTRASONIC_TRIG};
    use rppal::gpio::{Gpio, InputPin, Level, OutputPin, Trigger};

    /// How long the trigger is held high to start a ping. The part wants 10 µs.
    const TRIGGER_PULSE: Duration = Duration::from_micros(10);
    /// Settling time between pings, so the previous burst has died away before the
    /// next one goes out. Firing faster reads your own last ping.
    const BETWEEN_PINGS: Duration = Duration::from_millis(60);

    pub struct Ultrasonic {
        trig: OutputPin,
        echo: InputPin,
    }

    impl Ultrasonic {
        pub fn open() -> Result<Self, UltrasonicError> {
            let gpio = Gpio::new().map_err(|e| UltrasonicError::Gpio(e.to_string()))?;
            let mut trig = gpio
                .get(GPIO_ULTRASONIC_TRIG)
                .map_err(|e| UltrasonicError::Gpio(format!("GPIO{GPIO_ULTRASONIC_TRIG}: {e}")))?
                .into_output_low();
            trig.set_low();

            let mut echo = gpio
                .get(GPIO_ULTRASONIC_ECHO)
                .map_err(|e| UltrasonicError::Gpio(format!("GPIO{GPIO_ULTRASONIC_ECHO}: {e}")))?
                .into_input_pulldown();
            // Both edges: the pulse width between them is the measurement.
            echo.set_interrupt(Trigger::Both, None)
                .map_err(|e| UltrasonicError::Gpio(format!("echo interrupt: {e}")))?;

            Ok(Self { trig, echo })
        }

        /// Fire once and return the echo pulse width.
        fn ping(&mut self) -> Option<Duration> {
            // Clear anything cached from a previous burst before triggering, or the
            // first "rising" edge we see could be the tail of the last ping.
            let _ = self.echo.poll_interrupt(true, Some(Duration::ZERO));

            self.trig.set_high();
            std::thread::sleep(TRIGGER_PULSE);
            self.trig.set_low();

            let rise = self.wait_for(Level::High)?;
            let fall = self.wait_for(Level::Low)?;
            fall.checked_sub(rise)
        }

        /// Block for the next edge of the given polarity, returning its kernel timestamp.
        fn wait_for(&mut self, level: Level) -> Option<Duration> {
            let want = match level {
                Level::High => Trigger::RisingEdge,
                Level::Low => Trigger::FallingEdge,
            };
            loop {
                let event = self.echo.poll_interrupt(false, Some(ECHO_TIMEOUT)).ok()??;
                if event.trigger == want {
                    return Some(event.timestamp);
                }
                // An edge of the wrong polarity means we joined mid-pulse. Keep waiting
                // rather than treating it as the measurement.
            }
        }

        /// A full reading: several pings, combined.
        pub fn read_mm(&mut self, air_temp_c: Option<f32>) -> Option<f32> {
            let mut samples = Vec::with_capacity(SAMPLES);
            for i in 0..SAMPLES {
                if i > 0 {
                    std::thread::sleep(BETWEEN_PINGS);
                }
                if let Some(mm) = self.ping().and_then(|echo| distance_mm(echo, air_temp_c)) {
                    samples.push(mm);
                }
            }
            combine(&samples)
        }
    }
}

// --- Development machine --------------------------------------------------------------

#[cfg(not(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64"))))]
mod imp {
    use super::*;

    /// No desktop equivalent: this one genuinely needs the pins. `read_mm` returns
    /// `None`, which the capability model already reads as "not fitted" — the same
    /// thing a Pi with the sensor unplugged reports.
    pub struct Ultrasonic;

    impl Ultrasonic {
        pub fn open() -> Result<Self, UltrasonicError> {
            Err(UltrasonicError::Unsupported(std::env::consts::ARCH))
        }

        pub fn read_mm(&mut self, _air_temp_c: Option<f32>) -> Option<f32> {
            None
        }
    }
}

pub use imp::Ultrasonic;

#[cfg(test)]
mod tests {
    use super::*;

    /// Echo duration for a given distance, at 20 °C. The inverse of `distance_mm`.
    fn echo_for(mm: f32) -> Duration {
        let micros = f64::from(mm) * 2.0 / f64::from(speed_mm_per_us(Some(20.0)));
        Duration::from_nanos((micros * 1000.0) as u64)
    }

    #[test]
    fn a_known_pulse_converts_to_a_known_distance() {
        // 1 ms of echo at 20 °C: 1000 µs × 0.3434 mm/µs ÷ 2 ≈ 172 mm.
        let mm = distance_mm(Duration::from_micros(1000), Some(20.0)).unwrap();
        assert!((mm - 171.7).abs() < 0.5, "{mm}");
    }

    #[test]
    fn the_conversion_round_trips() {
        for expected in [25.0f32, 60.0, 150.0, 330.0, 900.0] {
            let mm = distance_mm(echo_for(expected), Some(20.0)).unwrap();
            assert!((mm - expected).abs() < 0.5, "{expected} -> {mm}");
        }
    }

    #[test]
    fn cold_air_would_read_the_tank_as_emptier_without_compensation() {
        // The reason temperature is worth using: it is a systematic bias, not noise.
        // Sound is slower when cold, so the same echo means a *nearer* surface — and
        // assuming 20 °C in a 5 °C room reports the water further away than it is.
        let echo = echo_for(300.0);
        let warm = distance_mm(echo, Some(30.0)).unwrap();
        let cold = distance_mm(echo, Some(5.0)).unwrap();

        assert!(cold < warm, "{cold} should be nearer than {warm}");
        assert!(
            (warm - cold) > 5.0,
            "the spread across a plausible room is {} mm and worth correcting",
            warm - cold
        );
    }

    #[test]
    fn a_missing_temperature_falls_back_rather_than_failing() {
        let echo = echo_for(300.0);
        let assumed = distance_mm(echo, None).unwrap();
        let stated = distance_mm(echo, Some(ASSUMED_TEMP_C)).unwrap();
        assert!((assumed - stated).abs() < 1e-3);
    }

    #[test]
    fn an_absurd_temperature_is_ignored_rather_than_trusted() {
        // A failing AM2320 can report nonsense before it reports nothing.
        for silly in [f32::NAN, -300.0, 5_000.0, f32::INFINITY] {
            let mm = distance_mm(echo_for(300.0), Some(silly)).unwrap();
            assert!((mm - 300.0).abs() < 1.0, "{silly} -> {mm}");
        }
    }

    #[test]
    fn readings_outside_the_sensors_range_are_rejected() {
        // Below: the sensor's own ring-down. Above: the ping went somewhere else.
        assert_eq!(distance_mm(Duration::from_micros(50), Some(20.0)), None);
        assert_eq!(distance_mm(Duration::from_millis(50), Some(20.0)), None);
        assert_eq!(distance_mm(Duration::ZERO, Some(20.0)), None);
    }

    #[test]
    fn the_median_ignores_a_scattered_ping() {
        // One reading twice the tank's depth must not move the answer. A mean of these
        // is 267 mm; the median is the truth.
        let samples = [298.0, 301.0, 299.0, 700.0, 300.0];
        let combined = combine(&samples).unwrap();
        assert!((combined - 300.0).abs() < 2.0, "{combined}");
    }

    #[test]
    fn a_burst_that_mostly_failed_produces_no_reading() {
        // Two good samples out of five is not a measurement, and reporting it would
        // put a number the rules trust behind a sensor that is barely answering.
        assert_eq!(combine(&[300.0, 301.0]), None);
        assert_eq!(combine(&[]), None);
        assert!(combine(&[300.0, 301.0, 299.0]).is_some());
    }

    #[test]
    fn out_of_range_samples_do_not_count_toward_the_quorum() {
        let mostly_junk = [300.0, 5.0, 9_000.0, f32::NAN, 301.0];
        assert_eq!(combine(&mostly_junk), None);
    }

    #[test]
    fn speed_of_sound_matches_the_textbook_figures() {
        assert!((speed_mm_per_us(Some(0.0)) * 1000.0 - 331.3).abs() < 0.1);
        assert!((speed_mm_per_us(Some(20.0)) * 1000.0 - 343.4).abs() < 0.2);
    }

    #[test]
    fn the_studio_tanks_whole_span_is_inside_the_sensor_range() {
        // 60 mm full to 330 mm empty. If either end fell outside the accepted range the
        // tank would silently stop reporting at one extreme.
        let geometry = garden_core::TankGeometry::STUDIO_2;
        for mm in [geometry.full_distance_mm, geometry.empty_distance_mm] {
            assert!(
                (MIN_RANGE_MM..=MAX_RANGE_MM).contains(&mm),
                "{mm} mm is outside the accepted range"
            );
            assert!(distance_mm(echo_for(mm), Some(20.0)).is_some());
        }
    }

    #[test]
    fn a_desktop_build_reports_no_sensor_rather_than_pretending() {
        // The mock backends elsewhere invent plausible readings; this one must not.
        // A fabricated water level would drive a real "add water" notification.
        #[cfg(not(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64"))))]
        assert!(Ultrasonic::open().is_err());
    }
}
