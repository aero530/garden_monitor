//! Parity capture: recording what the factory firmware does before we replace it.
//!
//! This is the one irreversible piece of Phase 1. The stock light curve — including
//! the sunrise/sunset ramp — and the pump duty cycle exist only inside the vendor
//! software, and the moment Phase 6 disables it that record is gone. Run this for a
//! week or two beforehand and the takeover has something to replicate rather than
//! something to guess at.
//!
//! Reads duty without a logic analyser, in preference order:
//!
//! 1. `pigs gdc <pin>` — the factory firmware is expected to drive PWM through
//!    pigpio, and pigpiod will report the duty cycle it was asked for.
//! 2. `/sys/class/pwm/...` — if the kernel PWM interface is used instead.
//!
//! Neither works if the vendor drives the pins some third way, in which case the
//! fallback is a jumper from the PWM line to a spare GPIO input. The CSV records which
//! source each sample came from so a run of `unavailable` is obvious rather than
//! looking like the lights were simply off.

use gardyn_proto::recon::expected;
use jiff::Timestamp;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Where a duty reading came from, recorded per sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Pigpio,
    SysfsPwm,
    Unavailable,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Pigpio => "pigpio",
            Source::SysfsPwm => "sysfs",
            Source::Unavailable => "unavailable",
        }
    }
}

/// A duty cycle in `0.0..=1.0`, and where it was read from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub duty: Option<f32>,
    pub source: Source,
}

impl Reading {
    pub fn unavailable() -> Self {
        Self {
            duty: None,
            source: Source::Unavailable,
        }
    }
}

/// `pigs gdc <pin>` returns the duty in the current range, `pigs prg <pin>` the range
/// itself. Both are needed: pigpio's default range is 255, not 100.
fn read_pigpio(pin: u8) -> Option<f32> {
    let duty: f32 = command_output("pigs", &["gdc", &pin.to_string()])?.trim().parse().ok()?;
    let range: f32 = command_output("pigs", &["prg", &pin.to_string()])?.trim().parse().ok()?;
    (range > 0.0).then(|| (duty / range).clamp(0.0, 1.0))
}

/// The kernel PWM interface exposes duty and period in nanoseconds.
fn read_sysfs(chip: u32, channel: u32) -> Option<f32> {
    let base = format!("/sys/class/pwm/pwmchip{chip}/pwm{channel}");
    if !Path::new(&base).exists() {
        return None;
    }
    let duty: f64 = std::fs::read_to_string(format!("{base}/duty_cycle"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let period: f64 = std::fs::read_to_string(format!("{base}/period"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (period > 0.0).then(|| (duty / period).clamp(0.0, 1.0) as f32)
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).to_string())
}

/// Read one pin, trying each source in turn.
pub fn read_duty(pin: u8, sysfs_channel: Option<u32>) -> Reading {
    if let Some(duty) = read_pigpio(pin) {
        return Reading {
            duty: Some(duty),
            source: Source::Pigpio,
        };
    }
    if let Some(channel) = sysfs_channel
        && let Some(duty) = read_sysfs(0, channel)
    {
        return Reading {
            duty: Some(duty),
            source: Source::SysfsPwm,
        };
    }
    Reading::unavailable()
}

fn csv_line(at: Timestamp, light: Reading, pump: Reading) -> String {
    let mut line = String::new();
    let render = |r: Reading| match r.duty {
        Some(d) => format!("{d:.4}"),
        None => String::new(),
    };
    let _ = write!(
        line,
        "{},{},{},{},{}",
        at,
        render(light),
        light.source.label(),
        render(pump),
        pump.source.label()
    );
    line
}

/// Sample both PWM pins on an interval, appending to a CSV.
///
/// Appends rather than truncates, and re-writes the header only for a new file, so a
/// reboot mid-capture does not cost the week of data already collected.
pub fn run_capture(out: &Path, interval: Duration) -> Result<(), Box<dyn std::error::Error>> {
    let is_new = !out.exists();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out)?;

    if is_new {
        writeln!(file, "at,light_duty,light_source,pump_duty,pump_source")?;
    }

    tracing::info!(
        "sampling GPIO{} (light) and GPIO{} (pump) every {:?} into {}",
        expected::GPIO_LIGHT,
        expected::GPIO_PUMP,
        interval,
        out.display()
    );
    tracing::info!("leave this running for a week or two, then commit the CSV");

    let mut warned = false;
    loop {
        // pwmchip0 channel 0 is GPIO18, channel 1 is GPIO19 on a Pi. The pump on
        // GPIO24 has no hardware PWM channel, so sysfs cannot see it and pigpio is
        // the only route.
        let light = read_duty(expected::GPIO_LIGHT, Some(0));
        let pump = read_duty(expected::GPIO_PUMP, None);

        if !warned && light.source == Source::Unavailable && pump.source == Source::Unavailable {
            tracing::warn!(
                "neither pin is readable — is pigpiod running? Without it this records \
                 nothing useful. See HARDWARE.md."
            );
            warned = true;
        }

        writeln!(file, "{}", csv_line(Timestamp::now(), light, pump))?;
        file.flush()?;
        std::thread::sleep(interval);
    }
}

/// Entry point used by `main`.
pub fn run(out: &Path, interval: Duration) -> Result<(), Box<dyn std::error::Error>> {
    run_capture(out, interval)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    #[test]
    fn a_sample_records_both_pins_and_their_sources() {
        let line = csv_line(
            t0(),
            Reading {
                duty: Some(0.82),
                source: Source::Pigpio,
            },
            Reading {
                duty: Some(0.25),
                source: Source::Pigpio,
            },
        );
        assert!(line.contains("0.8200"));
        assert!(line.contains("0.2500"));
        assert_eq!(line.matches("pigpio").count(), 2);
    }

    #[test]
    fn an_unreadable_pin_is_blank_rather_than_zero() {
        // A run of zeros would read as "the lights were off all week", which is a
        // completely different conclusion from "we could not see the pin".
        let line = csv_line(t0(), Reading::unavailable(), Reading::unavailable());
        assert!(line.contains(",,unavailable,,unavailable"), "{line}");
        assert!(!line.contains("0.0000"));
    }

    #[test]
    fn one_readable_pin_does_not_suppress_the_other() {
        let line = csv_line(
            t0(),
            Reading {
                duty: Some(1.0),
                source: Source::SysfsPwm,
            },
            Reading::unavailable(),
        );
        assert!(line.contains("1.0000,sysfs"));
        assert!(line.ends_with(",unavailable"));
    }

    #[test]
    fn the_header_matches_the_row_shape() {
        let header = "at,light_duty,light_source,pump_duty,pump_source";
        let row = csv_line(t0(), Reading::unavailable(), Reading::unavailable());
        assert_eq!(header.matches(',').count(), row.matches(',').count());
    }

    #[test]
    fn a_missing_pigpio_reports_unavailable_rather_than_panicking() {
        // On a development machine neither source exists.
        let reading = read_duty(expected::GPIO_LIGHT, None);
        if reading.source == Source::Unavailable {
            assert!(reading.duty.is_none());
        }
    }
}
