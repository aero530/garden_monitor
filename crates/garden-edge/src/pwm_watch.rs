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
//! 1. **pigpio's FIFO interface**, `/dev/pigpio` and `/dev/pigout`. This is the one
//!    that works on the Gardyn. Raspbian's stock unit starts the daemon as
//!    `pigpiod -l`, which disables the socket — so `pigs` answers `socket connect
//!    failed` from a daemon that is plainly running. It is also the cheap route: one
//!    write and one read per sample, against two `fork`/`exec`s for `pigs`, which is
//!    not nothing at 1 Hz on a single ARMv6 core.
//! 2. `pigs gdc <pin>` — for a device where the socket has been left enabled.
//! 3. `/sys/class/pwm/...` — if the kernel PWM interface is used instead.
//!
//! None works if the vendor drives the pins some fourth way, in which case the
//! fallback is a jumper from the PWM line to a spare GPIO input. The CSV records which
//! source each sample came from so a run of `unavailable` is obvious rather than
//! looking like the lights were simply off.
//!
//! **A duty of zero is not proof of darkness.** pigpiod reports what *it* was asked
//! for, so a pin driven by some other process reads as a confident, permanent 0.0000 —
//! the same trap as the `unavailable` run, wearing a more convincing disguise. Startup
//! logs each pin's mode next to its duty so the two can be compared against whether
//! the lights are physically on.

use garden_proto::recon::expected;
use jiff::Timestamp;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Where a duty reading came from, recorded per sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    PigpioFifo,
    /// Not a duty cycle at all: pigpio was not modulating the pin, so this is its
    /// level. `0.0000` from here means "sitting low", not "modulating at zero".
    PigpioLevel,
    Pigpio,
    SysfsPwm,
    Unavailable,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::PigpioFifo => "pigpio-fifo",
            Source::PigpioLevel => "pigpio-level",
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

/// pigpio's FIFO interface, which needs neither the socket nor root.
///
/// The permissions are the point: pigpio creates `/dev/pigpio` mode 0662 and
/// `/dev/pigout` mode 0664, so any user can write a command and read the answer. On a
/// Gardyn whose root password we do not have, this is the whole of Phase 1's access to
/// the pins.
#[cfg(unix)]
mod fifo {
    use std::fs::{File, OpenOptions};
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::{Duration, Instant};

    const INPUT: &str = "/dev/pigpio";
    const OUTPUT: &str = "/dev/pigout";

    /// `PI_NOT_PWM_GPIO`. Not a failure to read the pin: pigpio is telling us it is
    /// not modulating it. The Gardyn's pump answers this whenever it is idle.
    const NOT_PWM_GPIO: i32 = -92;

    /// How long to wait for a reply before giving up on it. Generous: the daemon
    /// answers a `gdc` in microseconds, and anything approaching this means it is
    /// wedged, which we want recorded as a gap rather than as a hang.
    const REPLY_TIMEOUT: Duration = Duration::from_millis(500);

    const MODE_INPUT: i32 = 0;

    /// pigpio's mode numbering, which is not the SoC's: `alt5` comes before `alt4`.
    pub fn mode_name(mode: i32) -> &'static str {
        match mode {
            MODE_INPUT => "input",
            1 => "output",
            2 => "alt5",
            3 => "alt4",
            4 => "alt0",
            5 => "alt1",
            6 => "alt2",
            7 => "alt3",
            _ => "unknown",
        }
    }

    /// What a pin is doing. The distinction is the point: pigpio saying "I am not
    /// modulating this" and pigpio being unreachable look identical if both collapse
    /// to `None`, and only one of them means the pump was off.
    pub enum Measured {
        /// pigpio is modulating the pin. This is the duty it was asked for.
        Pwm(f32),
        /// Nothing is modulating it, so the level is the whole story.
        Level(f32),
    }

    pub struct Pipe {
        input: File,
        output: File,
        buffer: Vec<u8>,
        /// Set when a command times out, cleared once the queue has been flushed.
        ///
        /// A command that gave up waiting has not necessarily been ignored: pigpiod may
        /// answer it late, and that answer then sits in the FIFO waiting to be read as
        /// the *next* command's reply. Every reading after that is offset by one — and
        /// because `measure` sends `gdc` then `prg`, an offset means dividing one
        /// number by another that was never its range. The result looks like a duty
        /// cycle rather than like an error, which is the failure this file spends most
        /// of its effort avoiding.
        ///
        /// Observed on the device on 2026-08-31: a light ramp issues 101 `hardware_PWM`
        /// calls in 1.5 s, pigpiod stops answering for long enough to trip the timeout,
        /// and the capture logs one `unavailable`. Roughly once per transition.
        desynced: bool,
    }

    impl Pipe {
        pub fn open() -> Option<Self> {
            // `O_NONBLOCK` on the write side is load-bearing. Opening a FIFO for
            // writing blocks until a reader appears, and pigpiod does not always
            // remove its FIFOs when it dies — so a stale `/dev/pigpio` would hang the
            // capture for ever instead of falling through to the next source. With the
            // flag, a live daemon (which holds both ends of both FIFOs open itself)
            // opens instantly and a dead one fails with `ENXIO`.
            let input = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(INPUT)
                .ok()?;
            let output = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(OUTPUT)
                .ok()?;

            let mut pipe = Self {
                input,
                output,
                buffer: Vec::new(),
                desynced: false,
            };
            // Replies queue in the FIFO until something drains them. Anything sitting
            // there now is the answer to somebody else's question, and reading it as
            // ours would offset every reply in the run by one.
            pipe.drain();
            Some(pipe)
        }

        /// Send one command and return its numeric reply.
        ///
        /// pigpio answers with a single line per command, in order: the result, or a
        /// negative error code. Callers check the sign — a bad GPIO is `-2`, not a
        /// duty cycle.
        pub fn command(&mut self, text: &str) -> Option<i32> {
            // Flushed here rather than at the moment of the timeout, so a reply that was
            // merely slow has had until now to turn up and be discarded.
            if self.desynced {
                self.drain();
                self.desynced = false;
            }

            writeln!(self.input, "{text}").ok()?;
            self.input.flush().ok()?;

            match self.reply() {
                Some(line) => line.parse().ok(),
                None => {
                    self.desynced = true;
                    None
                }
            }
        }

        /// What the pin is doing, as far as pigpio can tell.
        ///
        /// The range is read every time rather than cached, even though it changes
        /// rarely and this doubles the traffic on a FIFO we share with the factory
        /// firmware. A cached range is silently wrong across a switch between hardware
        /// PWM (range 1,000,000) and software PWM (range 255) — a duty of 128 would
        /// record as 0.0001 instead of 0.5, and a parity capture whose whole purpose is
        /// fidelity cannot afford a plausible wrong number.
        pub fn measure(&mut self, pin: u8) -> Option<Measured> {
            let duty = self.command(&format!("gdc {pin}"))?;

            if duty == NOT_PWM_GPIO {
                // Nothing is modulating the pin, which for one the Pi drives is an
                // answer rather than a gap: an idle pump is genuinely off. For an
                // input the level is somebody else's signal, so leave it blank.
                let mode = self.command(&format!("mg {pin}"))?;
                if mode == MODE_INPUT {
                    return None;
                }
                let level = self.command(&format!("r {pin}"))?;
                return match level {
                    0 | 1 => Some(Measured::Level(level as f32)),
                    _ => None,
                };
            }

            let range = self.command(&format!("prg {pin}"))?;
            (duty >= 0 && range > 0)
                .then(|| Measured::Pwm((duty as f32 / range as f32).clamp(0.0, 1.0)))
        }

        /// The pin's function, read from the SoC's own function-select register rather
        /// than from pigpio's idea of what it set — so it is true even for a pin some
        /// other process configured.
        pub fn mode(&mut self, pin: u8) -> Option<&'static str> {
            Some(mode_name(self.command(&format!("mg {pin}"))?))
        }

        fn reply(&mut self) -> Option<String> {
            let deadline = Instant::now() + REPLY_TIMEOUT;
            loop {
                if let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = self.buffer.drain(..=end).collect();
                    return String::from_utf8(line).ok().map(|s| s.trim().to_string());
                }
                let mut chunk = [0u8; 256];
                match self.output.read(&mut chunk) {
                    Ok(0) => {}
                    Ok(read) => {
                        self.buffer.extend_from_slice(&chunk[..read]);
                        continue;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_) => return None,
                }
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn drain(&mut self) {
            let mut chunk = [0u8; 256];
            while let Ok(read) = self.output.read(&mut chunk) {
                if read == 0 {
                    break;
                }
            }
            self.buffer.clear();
        }
    }
}

/// The FIFOs are a Unix thing; on a development machine there is nothing to open.
#[cfg(not(unix))]
mod fifo {
    // Nothing here is ever constructed — `open` returns `None` — but the shapes have
    // to exist for the caller to compile against on a development machine.
    #[allow(dead_code)]
    pub enum Measured {
        Pwm(f32),
        Level(f32),
    }

    pub struct Pipe;

    impl Pipe {
        pub fn open() -> Option<Self> {
            None
        }
        pub fn command(&mut self, _text: &str) -> Option<i32> {
            None
        }
        pub fn measure(&mut self, _pin: u8) -> Option<Measured> {
            None
        }
        pub fn mode(&mut self, _pin: u8) -> Option<&'static str> {
            None
        }
    }
}

/// Every GPIO's function, for the recon report.
///
/// This is what `raspi-gpio funcs` would give you, except that `raspi-gpio` wants root
/// and pigpio's FIFO does not — which on this device is the difference between having
/// the pin map and not.
pub fn pin_modes(pins: impl IntoIterator<Item = u8>) -> Vec<(u8, &'static str)> {
    let Some(mut pipe) = fifo::Pipe::open() else {
        return Vec::new();
    };
    pins.into_iter()
        .filter_map(|pin| Some((pin, pipe.mode(pin)?)))
        .collect()
}

/// Refuses to start a second capture while one is already running.
///
/// Two `watch-pwm` processes share one `/dev/pigout`, and a FIFO is a byte stream with
/// no addressing: they consume each other's replies, and sometimes each other's
/// half-lines. Observed on the device on 2026-08-31 — two instances produced a CSV in
/// which a fragment of one pin's duty was divided by another pin's range, giving
/// `0.0005` under source `pigpio-fifo`. In range, plausibly shaped, and entirely
/// fictitious. The `unavailable` rows interleaved with it were the honest half of the
/// same collision, and the give-away was the sample interval: 0.5 s from a loop that
/// sleeps 1 s, because two cadences were interleaving into one file.
///
/// This module exists because that failure is silent in the one artefact that cannot be
/// re-collected after Phase 6.
#[cfg(unix)]
mod lock {
    use std::fs::OpenOptions;
    use std::io::{ErrorKind, Write};
    use std::path::{Path, PathBuf};

    /// Held for the life of the capture; the file is removed on the way out.
    pub struct Guard {
        path: PathBuf,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn lock_path() -> PathBuf {
        std::env::temp_dir().join("garden-edge-watch-pwm.lock")
    }

    /// Take the lock, or report which process already holds it.
    pub fn acquire() -> Result<Guard, String> {
        let path = lock_path();
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let _ = write!(file, "{}", std::process::id());
                    return Ok(Guard { path });
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    let holder = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| s.trim().parse::<u32>().ok());
                    // A lock file outlives a process that was killed rather than
                    // stopped, and refusing forever because of a crash three weeks ago
                    // would be its own failure.
                    match holder.filter(|pid| Path::new(&format!("/proc/{pid}")).exists()) {
                        Some(pid) => return Err(format!("process {pid}")),
                        None => {
                            let _ = std::fs::remove_file(&path);
                            continue;
                        }
                    }
                }
                // An unwritable temp directory is not a reason to refuse to capture.
                Err(_) => return Ok(Guard { path }),
            }
        }
        Err("another process".to_string())
    }
}

#[cfg(not(unix))]
mod lock {
    pub struct Guard;
    pub fn acquire() -> Result<Guard, String> {
        Ok(Guard)
    }
}

/// Which pigpio interface actually answers, if any.
///
/// `probe` uses this rather than `pgrep pigpiod`, because on this device the two
/// disagree: the daemon is up and the socket is shut. `hwver` is the cheapest thing
/// pigpio will answer and it touches no pin.
pub fn pigpio_interface() -> Option<&'static str> {
    if let Some(mut pipe) = fifo::Pipe::open()
        && pipe.command("hwver").is_some_and(|version| version > 0)
    {
        return Some("FIFO (/dev/pigpio)");
    }
    command_output("pigs", &["hwver"]).map(|_| "socket (pigs)")
}

/// Holds the FIFO open across samples, so the common case costs one write and one
/// read rather than two processes.
pub struct DutyReader {
    pipe: Option<fifo::Pipe>,
}

impl DutyReader {
    /// Opens what it can and says what it found.
    ///
    /// The startup line is deliberately noisy. Getting this wrong costs a fortnight,
    /// and the difference between a working capture and a useless one is visible here
    /// and nowhere else until the CSV is opened.
    pub fn new() -> Self {
        let mut reader = Self {
            pipe: fifo::Pipe::open(),
        };
        let Some(pipe) = reader.pipe.as_mut() else {
            tracing::info!("pigpio FIFO not available; falling back to `pigs` and sysfs");
            return reader;
        };

        tracing::info!("reading duty over pigpio's FIFO interface, no root required");
        let mut modulated = false;
        for (pin, what) in [
            (expected::GPIO_LIGHT, "light"),
            (expected::GPIO_PUMP, "pump"),
        ] {
            let mode = pipe.mode(pin).unwrap_or("unknown");
            let frequency = pipe.command(&format!("pfg {pin}")).unwrap_or(-1);
            let state = match pipe.measure(pin) {
                Some(fifo::Measured::Pwm(duty)) => {
                    modulated = true;
                    format!("duty {duty:.4} at {frequency} Hz")
                }
                Some(fifo::Measured::Level(level)) => {
                    format!(
                        "not modulated, sitting {}",
                        if level > 0.5 { "high" } else { "low" }
                    )
                }
                None => "unreadable".to_string(),
            };
            tracing::info!("  GPIO{pin} ({what}): mode {mode}, {state}");
        }

        if !modulated {
            tracing::warn!(
                "pigpio is not modulating either pin. That is normal for a pump between \
                 cycles, but if the lights are physically on then the factory firmware \
                 is driving them without going through pigpiod, and this capture will \
                 record a fortnight of zeros. Check before walking away."
            );
        }
        reader
    }

    /// Read one pin, trying each source in turn.
    pub fn read(&mut self, pin: u8, sysfs_channel: Option<u32>) -> Reading {
        if let Some(pipe) = self.pipe.as_mut() {
            match pipe.measure(pin) {
                Some(fifo::Measured::Pwm(duty)) => {
                    return Reading {
                        duty: Some(duty),
                        source: Source::PigpioFifo,
                    };
                }
                // Recorded under its own source rather than as a plain 0.0000, so that
                // "the pump was idle" and "the pump was modulating at zero" stay
                // distinguishable in the CSV months later.
                Some(fifo::Measured::Level(level)) => {
                    return Reading {
                        duty: Some(level),
                        source: Source::PigpioLevel,
                    };
                }
                None => {}
            }
        }
        read_duty(pin, sysfs_channel)
    }
}

impl Default for DutyReader {
    fn default() -> Self {
        Self::new()
    }
}

/// `pigs gdc <pin>` returns the duty in the current range, `pigs prg <pin>` the range
/// itself. Both are needed: pigpio's default range is 255, not 100.
fn read_pigpio(pin: u8) -> Option<f32> {
    let duty: f32 = command_output("pigs", &["gdc", &pin.to_string()])?
        .trim()
        .parse()
        .ok()?;
    let range: f32 = command_output("pigs", &["prg", &pin.to_string()])?
        .trim()
        .parse()
        .ok()?;
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
    // Before the file is opened, so a refused second instance cannot even append a
    // header to somebody else's capture.
    let _lock = lock::acquire().map_err(|holder| {
        format!(
            "another garden-edge watch-pwm is already running ({holder}). Two of them \
             share one /dev/pigout and read each other's replies, which produces \
             plausible-looking duty cycles that are arithmetic between two different \
             pins. Stop the other one first: pgrep -af 'garden-edge watch-pwm'"
        )
    })?;

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

    let mut reader = DutyReader::new();
    let mut warned = false;
    loop {
        // pwmchip0 channel 0 is GPIO18, channel 1 is GPIO19 on a Pi. The pump on
        // GPIO24 has no hardware PWM channel, so sysfs cannot see it and pigpio is
        // the only route.
        let light = reader.read(expected::GPIO_LIGHT, Some(0));
        let pump = reader.read(expected::GPIO_PUMP, None);

        if !warned && light.source == Source::Unavailable && pump.source == Source::Unavailable {
            // "Is pigpiod running?" is the wrong first question on this device: it is,
            // and `pigs` still cannot reach it. Point at the daemon's arguments, which
            // is where the answer actually lives.
            tracing::warn!(
                "neither pin is readable — this run will record nothing. Check \
                 /dev/pigpio and /dev/pigout exist and that pigpiod is up; `pigs` \
                 failing on its own means nothing, because Raspbian starts the daemon \
                 with -l and the socket it speaks is disabled. See HARDWARE.md."
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
    fn every_source_is_distinguishable_in_the_csv() {
        // Which interface a reading came from is the first thing anyone asks of a
        // suspicious run, and a shared label would make two of them indistinguishable
        // after the fact.
        let labels = [
            Source::PigpioFifo,
            Source::PigpioLevel,
            Source::Pigpio,
            Source::SysfsPwm,
            Source::Unavailable,
        ]
        .map(Source::label);
        let mut unique = labels.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len());
    }

    #[test]
    fn the_fifo_and_the_socket_are_recorded_as_different_sources() {
        // They report the same number from the same daemon, but only one of them
        // works on this Gardyn. If the socket ever starts answering, the CSV should
        // show where the change happened.
        let line = csv_line(
            t0(),
            Reading {
                duty: Some(0.5),
                source: Source::PigpioFifo,
            },
            Reading {
                duty: Some(0.25),
                source: Source::Pigpio,
            },
        );
        assert!(line.contains("0.5000,pigpio-fifo"), "{line}");
        assert!(line.ends_with("0.2500,pigpio"), "{line}");
    }

    #[test]
    fn an_idle_pump_is_not_recorded_the_same_way_as_one_modulating_at_zero() {
        // pigpio answers -92 for a pin it is not modulating, which is the Gardyn's
        // pump between cycles. Both end up 0.0000 in the duty column, and the source
        // is the only thing that says which happened.
        let idle = csv_line(
            t0(),
            Reading::unavailable(),
            Reading {
                duty: Some(0.0),
                source: Source::PigpioLevel,
            },
        );
        let modulating_at_zero = csv_line(
            t0(),
            Reading::unavailable(),
            Reading {
                duty: Some(0.0),
                source: Source::PigpioFifo,
            },
        );
        assert_ne!(idle, modulating_at_zero);
        assert!(idle.ends_with("0.0000,pigpio-level"), "{idle}");
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
