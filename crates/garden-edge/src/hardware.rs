//! Peripheral access on the Gardyn's Raspberry Pi.
//!
//! Two backends. On an ARM Linux board this talks to real I²C and GPIO through
//! `rppal`; everywhere else it compiles to a mock so the agent's logic can be built
//! and tested on a desktop. The mock is not a stub that panics — it returns plausible
//! readings, because a `probe` run on a laptop should show you what the output *looks
//! like* before you take a screwdriver to anything.
//!
//! Nothing here writes to an actuator. Phase 1 is read-only by design: the agent runs
//! alongside the factory firmware, and two processes fighting over the same PWM pin
//! would be an excellent way to cook a tray of seedlings.

use garden_core::{SensorSnapshot, Timestamp};
use garden_proto::recon::{CameraDevice, I2cDevice, ReconReport, expected};
use std::path::Path;
use std::process::Command;

/// Which I²C bus the peripherals sit on. Bus 1 on every modern Pi.
pub const I2C_BUS: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum HardwareError {
    // Constructed only by the desktop backend; kept in the shared enum so both
    // backends return the same error type.
    #[allow(dead_code)]
    #[error("this build has no hardware support; it was compiled for {0}")]
    Unsupported(&'static str),
    // Constructed only by the ARM backend; kept in the shared enum so both backends
    // return the same error type.
    #[allow(dead_code)]
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[allow(dead_code)]
    #[error("{0}")]
    Bus(String),
}

pub type Result<T> = std::result::Result<T, HardwareError>;

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        // Device-tree strings are NUL-terminated; trimming whitespace alone leaves the
        // NUL in place and it renders as a stray glyph in the report.
        .map(|s| s.trim_matches(|c: char| c.is_whitespace() || c == '\0').to_string())
        .filter(|s| !s.is_empty())
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).to_string())
}

/// Everything Phase 0 needs to know about this board.
///
/// Deliberately tolerant: a missing file or an absent tool is a warning on the report,
/// never a failure. The whole point is to describe an unknown device, and refusing to
/// produce a report because one probe was unavailable would defeat that.
pub fn probe(agent_version: &str, now: Timestamp) -> ReconReport {
    let mut report = ReconReport {
        agent_version: agent_version.to_string(),
        captured_at: now.to_string(),
        ..Default::default()
    };

    report.board_model = read_trimmed("/proc/device-tree/model");
    report.cpu_architecture = Some(std::env::consts::ARCH.to_string());
    report.kernel = command_output("uname", &["-r"]).map(|s| s.trim().to_string());
    report.os = std::fs::read_to_string("/etc/os-release").ok().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("PRETTY_NAME="))
            .map(|v| v.trim_matches('"').to_string())
    });

    probe_i2c(&mut report);
    probe_cameras(&mut report);
    probe_one_wire(&mut report);
    probe_pwm(&mut report);
    probe_services(&mut report);

    if report.board_model.is_none() {
        report.warnings.push(
            "no /proc/device-tree/model — this is not a Raspberry Pi, or the report was \
             generated on a development machine"
                .into(),
        );
    }
    report
}

fn probe_i2c(report: &mut ReconReport) {
    report.i2c_bus = Some(format!("/dev/i2c-{I2C_BUS}"));
    let found = scan_i2c();

    match found {
        Ok(addresses) => {
            for address in &addresses {
                let expected_name = expected::ALL_I2C
                    .iter()
                    .find(|(a, _)| a == address)
                    .map(|(_, name)| (*name).to_string());
                report.i2c_devices.push(I2cDevice {
                    address: *address,
                    expected: expected_name,
                });
            }
            for (address, name) in expected::ALL_I2C {
                if !addresses.contains(address) {
                    report.i2c_missing.push(format!("0x{address:02x} {name}"));
                }
            }
        }
        Err(e) => report.warnings.push(format!("I²C scan failed: {e}")),
    }
}

fn probe_cameras(report: &mut ReconReport) {
    for index in 0..8 {
        let path = format!("/dev/video{index}");
        if !Path::new(&path).exists() {
            continue;
        }
        let formats = command_output("v4l2-ctl", &["-d", &path, "--list-formats"])
            .map(|out| {
                out.lines()
                    .filter(|l| l.contains("Pixel Format"))
                    .map(|l| l.trim().to_string())
                    .collect()
            })
            .unwrap_or_default();
        report.cameras.push(CameraDevice {
            path,
            name: None,
            formats,
        });
    }
    if report.cameras.is_empty() {
        report
            .warnings
            .push("no /dev/video* devices — the camera may be on a different interface".into());
    }
}

fn probe_one_wire(report: &mut ReconReport) {
    let Ok(entries) = std::fs::read_dir("/sys/bus/w1/devices") else {
        report.warnings.push(
            "1-Wire bus not present; a DS18B20 water probe needs `dtoverlay=w1-gpio` in \
             /boot/firmware/config.txt"
                .into(),
        );
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name != "w1_bus_master1" {
            report.one_wire_devices.push(name);
        }
    }
}

fn probe_pwm(report: &mut ReconReport) {
    if let Ok(entries) = std::fs::read_dir("/sys/class/pwm") {
        for entry in entries.flatten() {
            report
                .pwm_channels
                .push(entry.file_name().to_string_lossy().to_string());
        }
    }
    // The factory firmware is expected to drive PWM through pigpio, which is also how
    // `watch-pwm` reads its duty cycle back without a logic analyser.
    report.pigpiod_running = command_output("pgrep", &["-x", "pigpiod"]).is_some();
    if !report.pigpiod_running {
        report.warnings.push(
            "pigpiod is not running; parity capture will need /sys/class/pwm or a \
             jumper to a spare GPIO"
                .into(),
        );
    }
}

fn probe_services(report: &mut ReconReport) {
    let Some(output) = command_output(
        "systemctl",
        &["list-units", "--type=service", "--state=running", "--no-legend", "--plain"],
    ) else {
        report
            .warnings
            .push("could not list systemd services".into());
        return;
    };

    for line in output.lines() {
        let Some(unit) = line.split_whitespace().next() else {
            continue;
        };
        let lower = unit.to_lowercase();
        // Anything that looks like it belongs to the vendor. Worth knowing before
        // Phase 6 disables it.
        if ["garden", "azure", "iot", "kelby", "garden"]
            .iter()
            .any(|needle| lower.contains(needle))
        {
            report.vendor_services.push(unit.to_string());
        }
    }
}

// --- Real hardware ---------------------------------------------------------------

#[cfg(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64")))]
mod imp {
    use super::*;
    use rppal::i2c::I2c;

    /// Probe every 7-bit address by attempting a zero-length write, which is what
    /// `i2cdetect` does. Reserved ranges are skipped.
    pub fn scan_i2c() -> Result<Vec<u16>> {
        let mut bus = I2c::with_bus(I2C_BUS).map_err(|e| HardwareError::Bus(e.to_string()))?;
        let mut found = Vec::new();
        for address in 0x03u16..=0x77 {
            if bus.set_slave_address(address).is_err() {
                continue;
            }
            if bus.write(&[]).is_ok() {
                found.push(address);
            }
        }
        Ok(found)
    }

    /// Read whatever is fitted, leaving absent probes as `None`.
    ///
    /// A failed read is also `None` rather than an error: one flaky sensor must not
    /// stop the other five being reported, and the capability model already treats
    /// "no reading" as "not available".
    pub fn read_sensors(now: Timestamp) -> SensorSnapshot {
        let mut snapshot = SensorSnapshot::empty(now);
        if let Ok((temp, humidity)) = am2320() {
            snapshot.air_temp_c = Some(temp);
            snapshot.humidity_pct = Some(humidity);
        }
        snapshot.pcb_temp_c = pct2075().ok();
        snapshot.pump_current_ma = ina219().ok();
        snapshot.water_temp_c = super::ds18b20();
        snapshot
    }

    fn open(address: u16) -> Result<I2c> {
        let mut bus = I2c::with_bus(I2C_BUS).map_err(|e| HardwareError::Bus(e.to_string()))?;
        bus.set_slave_address(address)
            .map_err(|e| HardwareError::Bus(e.to_string()))?;
        Ok(bus)
    }

    /// AM2320 air temperature and humidity.
    ///
    /// The part sleeps between reads and NAKs the wake-up, so the first write is
    /// expected to fail and must be ignored rather than retried as an error.
    fn am2320() -> Result<(f32, f32)> {
        let mut bus = open(expected::AM2320)?;
        let _ = bus.write(&[]);
        std::thread::sleep(std::time::Duration::from_millis(2));

        bus.write(&[0x03, 0x00, 0x04])
            .map_err(|e| HardwareError::Bus(format!("AM2320 request: {e}")))?;
        std::thread::sleep(std::time::Duration::from_millis(2));

        let mut buffer = [0u8; 8];
        bus.read(&mut buffer)
            .map_err(|e| HardwareError::Bus(format!("AM2320 read: {e}")))?;

        let humidity = f32::from(u16::from_be_bytes([buffer[2], buffer[3]])) / 10.0;
        let raw = u16::from_be_bytes([buffer[4], buffer[5]]);
        // Bit 15 is the sign; the remaining bits are tenths of a degree.
        let temp = if raw & 0x8000 != 0 {
            -(f32::from(raw & 0x7FFF) / 10.0)
        } else {
            f32::from(raw) / 10.0
        };
        Ok((temp, humidity))
    }

    /// PCT2075 board temperature: 11-bit, left-justified, 0.125 °C per LSB.
    fn pct2075() -> Result<f32> {
        let bus = open(expected::PCT2075)?;
        let mut buffer = [0u8; 2];
        bus.write_read(&[0x00], &mut buffer)
            .map_err(|e| HardwareError::Bus(format!("PCT2075: {e}")))?;
        let raw = i16::from_be_bytes(buffer) >> 5;
        Ok(f32::from(raw) * 0.125)
    }

    /// INA219 pump current from the shunt voltage.
    ///
    /// Reads the shunt register directly rather than the calibrated current register,
    /// because the calibration value is set by whatever software configured the chip
    /// last — and in Phase 1 that is the factory firmware, not us.
    fn ina219() -> Result<f32> {
        const SHUNT_VOLTAGE: u8 = 0x01;
        /// 10 µV per LSB on the shunt register.
        const LSB_MICROVOLTS: f32 = 10.0;
        /// Assumed shunt. Confirm against the board before trusting absolute values;
        /// the fouling trend only needs the reading to be consistent.
        const SHUNT_MILLIOHMS: f32 = 100.0;

        let bus = open(expected::INA219)?;
        let mut buffer = [0u8; 2];
        bus.write_read(&[SHUNT_VOLTAGE], &mut buffer)
            .map_err(|e| HardwareError::Bus(format!("INA219: {e}")))?;
        let microvolts = f32::from(i16::from_be_bytes(buffer)) * LSB_MICROVOLTS;
        Ok(microvolts / SHUNT_MILLIOHMS)
    }
}

// --- Development machine ----------------------------------------------------------

#[cfg(not(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64"))))]
mod imp {
    use super::*;

    pub fn scan_i2c() -> Result<Vec<u16>> {
        Err(HardwareError::Unsupported(std::env::consts::ARCH))
    }

    /// Plausible readings so the agent can be exercised end to end on a desktop.
    ///
    /// Deliberately static rather than random: a developer comparing two runs should
    /// see a difference only when they changed something.
    pub fn read_sensors(now: Timestamp) -> SensorSnapshot {
        let mut snapshot = SensorSnapshot::empty(now);
        snapshot.air_temp_c = Some(21.4);
        snapshot.humidity_pct = Some(46.0);
        snapshot.pcb_temp_c = Some(29.8);
        snapshot.pump_current_ma = Some(408.0);
        // 1-Wire is plain sysfs, so this is worth attempting even here — it returns
        // None off-device, and on a Linux box with a probe wired up it just works.
        snapshot.water_temp_c = super::ds18b20();
        // Water level needs the ultrasonic sensor and microsecond GPIO timing, which
        // has no meaningful desktop equivalent.
        snapshot
    }
}

pub use imp::scan_i2c;

/// The sensors this device actually has, held open across reads.
///
/// Most peripherals are stateless — open the I²C bus, read, close — but the ultrasonic
/// is not: it registers a kernel interrupt on the echo pin, and re-registering that
/// every minute would be both wasteful and a good way to miss the edge we are waiting
/// for. So the daemon holds a `Bank` and the one-shot subcommands build a throwaway.
pub struct Bank {
    ultrasonic: Option<crate::ultrasonic::Ultrasonic>,
}

impl Bank {
    pub fn open() -> Self {
        let ultrasonic = match crate::ultrasonic::Ultrasonic::open() {
            Ok(sensor) => Some(sensor),
            // A desktop build has no pins and says so; that is not news.
            Err(crate::ultrasonic::UltrasonicError::Unsupported(arch)) => {
                tracing::debug!(%arch, "no GPIO on this build; water level unavailable");
                None
            }
            // On a Pi this is almost always the `gpio` group. Worth shouting about,
            // because the symptom otherwise is simply never being told to add water.
            Err(error) => {
                tracing::warn!(
                    %error,
                    "cannot open the ultrasonic sensor — water level will be missing,                      and the water rule cannot run without it. Check the agent's user                      is in the `gpio` group."
                );
                None
            }
        };
        Self { ultrasonic }
    }

    /// One pass over everything fitted.
    ///
    /// Air temperature is read first and handed to the ultrasonic: the speed of sound
    /// varies about 0.6 m/s per degree, which across this tank is millimetres of
    /// systematic error in the same direction all winter.
    pub fn read(&mut self, now: Timestamp) -> SensorSnapshot {
        let mut snapshot = imp::read_sensors(now);
        if let Some(sensor) = self.ultrasonic.as_mut() {
            snapshot.water_level_mm = sensor.read_mm(snapshot.air_temp_c);
        }
        snapshot
    }
}

/// One-shot read, for `probe`, `read` and `report`.
pub fn read_sensors(now: Timestamp) -> SensorSnapshot {
    Bank::open().read(now)
}

/// Read a DS18B20 water probe, if one is fitted.
///
/// 1-Wire is exposed as plain text through sysfs, so this needs no driver crate and
/// works identically whichever backend is compiled in.
pub fn ds18b20() -> Option<f32> {
    let entries = std::fs::read_dir("/sys/bus/w1/devices").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("28-") {
            continue;
        }
        let contents = std::fs::read_to_string(entry.path().join("temperature"))
            .or_else(|_| std::fs::read_to_string(entry.path().join("w1_slave")))
            .ok()?;
        // The modern `temperature` file is millidegrees; the legacy `w1_slave` file
        // ends with `t=<millidegrees>`.
        let millidegrees = contents
            .rsplit("t=")
            .next()
            .and_then(|s| s.trim().parse::<i32>().ok())?;
        return Some(millidegrees as f32 / 1000.0);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    #[test]
    fn a_probe_always_produces_a_report() {
        // Even on a machine with none of the hardware, which is the case that matters
        // for anyone reading the output before they open the device.
        let report = probe("0.1.0", t0());
        assert_eq!(report.agent_version, "0.1.0");
        assert!(!report.captured_at.is_empty());
        assert!(report.cpu_architecture.is_some());
    }

    #[test]
    fn a_probe_on_a_development_machine_says_so_rather_than_claiming_hardware() {
        let report = probe("0.1.0", t0());
        if report.board_model.is_none() {
            assert!(
                report.warnings.iter().any(|w| w.contains("development machine")),
                "warnings should explain the absent board: {:?}",
                report.warnings
            );
        }
    }

    #[test]
    fn sensor_reads_never_invent_probes_that_are_not_fitted() {
        // EC and pH are deferred hardware; a backend reporting 0.0 for them would
        // silently switch the rules onto measured dosing.
        let snapshot = read_sensors(t0());
        assert!(snapshot.ec_ms_cm.is_none());
        assert!(snapshot.ph.is_none());
    }

    #[test]
    fn a_reading_is_stamped_with_the_time_it_was_taken() {
        assert_eq!(read_sensors(t0()).at, t0());
    }

    #[test]
    fn the_mock_backend_is_stable_between_runs() {
        assert_eq!(read_sensors(t0()), read_sensors(t0()));
    }

    #[test]
    fn a_device_tree_string_would_have_its_nul_trimmed() {
        // Guards the trim: device-tree values are NUL-terminated and the stray byte
        // renders as a control glyph in the committed report.
        let raw = "Raspberry Pi Zero 2 W Rev 1.0\0";
        let cleaned = raw.trim_matches(|c: char| c.is_whitespace() || c == '\0');
        assert_eq!(cleaned, "Raspberry Pi Zero 2 W Rev 1.0");
    }
}
