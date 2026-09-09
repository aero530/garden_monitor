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
use garden_proto::recon::{CameraDevice, GpioPin, I2cDevice, ReconReport, expected};
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

/// Decode the AHT20's six-byte measurement into `(°C, %RH)`.
///
/// Two 20-bit values sharing the middle byte: humidity in the high nibble of byte 3,
/// temperature in the low. Both are fractions of 2²⁰ — humidity over 100, temperature
/// over 200 with a 50 °C offset — which is what distinguishes this part from the AM2320
/// the peripheral map claimed, whose readings come in tenths of a degree.
///
/// Pure, so the arithmetic is checked on a desktop against the factory's own reported
/// values rather than against a datasheet reading of it.
/// Only the ARM backend has an I²C bus to call this with, so off-target its only
/// caller is its own test suite — which is the point of splitting it out.
#[cfg_attr(
    not(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64"))),
    allow(dead_code)
)]
fn aht20_convert(buffer: &[u8; 6]) -> Option<(f32, f32)> {
    /// Bit 7 of the status byte. Set means the conversion is still running and the rest
    /// of the buffer is the *previous* measurement — stale rather than wrong, but this
    /// part is shared with the factory's own reader, so a busy flag is worth honouring.
    const BUSY: u8 = 0x80;
    /// 2²⁰. Both fields are twenty bits.
    const FULL_SCALE: f32 = 1_048_576.0;

    if buffer[0] & BUSY != 0 {
        return None;
    }

    let humidity_raw =
        (u32::from(buffer[1]) << 12) | (u32::from(buffer[2]) << 4) | (u32::from(buffer[3]) >> 4);
    let temp_raw =
        ((u32::from(buffer[3]) & 0x0F) << 16) | (u32::from(buffer[4]) << 8) | u32::from(buffer[5]);

    let humidity = humidity_raw as f32 / FULL_SCALE * 100.0;
    let temp = temp_raw as f32 / FULL_SCALE * 200.0 - 50.0;

    // An uncalibrated or absent part reads as all zeros or all ones, which converts to
    // -50 °C at 0% or 150 °C at 100%. Neither is a room, and either would be worse than
    // no reading: the capability model handles absence, but a rule cannot tell that a
    // plausible-looking number is fiction.
    if !(-40.0..=85.0).contains(&temp) || !(0.0..=100.0).contains(&humidity) {
        return None;
    }
    Some((temp, humidity))
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        // Device-tree strings are NUL-terminated; trimming whitespace alone leaves the
        // NUL in place and it renders as a stray glyph in the report.
        .map(|s| {
            s.trim_matches(|c: char| c.is_whitespace() || c == '\0')
                .to_string()
        })
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
    report.os = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
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
             the boot config — /boot/config.txt on this Pi's Raspbian 9, \
             /boot/firmware/config.txt on Bookworm and later"
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
    // Running is not the same as reachable, and on this device the two disagree:
    // Raspbian's stock unit is `pigpiod -l`, which shuts the socket `pigs` speaks and
    // leaves only the FIFOs. Only reachability predicts whether parity capture will
    // record anything, so ask the daemon rather than the process table.
    report.pigpiod_interface = crate::pwm_watch::pigpio_interface().map(str::to_string);

    // The whole header, not just the four documented pins: the spare ones are where a
    // DS18B20 or a parity jumper goes, and knowing which are spare needs the same scan.
    report.gpio_modes = crate::pwm_watch::pin_modes(0..=27)
        .into_iter()
        .map(|(gpio, mode)| GpioPin {
            gpio,
            mode: mode.to_string(),
            role: expected::PIN_ROLES
                .iter()
                .find(|(pin, _, _)| *pin == gpio)
                .map(|(_, role, _)| (*role).to_string()),
        })
        .collect();
    for conflict in report.pin_role_conflicts() {
        report.warnings.push(conflict);
    }

    match (report.pigpiod_running, report.pigpiod_interface.is_some()) {
        (_, true) => {}
        (true, false) => report.warnings.push(
            "pigpiod is running but answers on neither its socket nor its FIFOs. \
             Parity capture would log `unavailable` for weeks. Check that /dev/pigpio \
             and /dev/pigout exist before starting Phase 1"
                .into(),
        ),
        (false, false) => report.warnings.push(
            "pigpiod is not running; parity capture will need /sys/class/pwm or a \
             jumper to a spare GPIO"
                .into(),
        ),
    }
}

fn probe_services(report: &mut ReconReport) {
    let Some(output) = command_output(
        "systemctl",
        &[
            "list-units",
            "--type=service",
            "--state=running",
            "--no-legend",
            "--plain",
        ],
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
        //
        // The first two needles are the ones that matter: Phase 0 on the real unit
        // found the sensor and water-level daemons named `gy_events` and `gy_wl`, and
        // the connection-string and pairing helpers with no vendor mark on them at
        // all. A guess at "garden" or "gardyn" would have listed none of them, which
        // reads as a clean device rather than a missed one.
        const VENDOR: &[&str] = &[
            "gy_",             // gy_events, gy_wl, gy_iot
            "iot",             // gy_iot, iot-controller
            "conn-string",     // Azure connection string provisioning
            "wifi-pairing",    // vendor onboarding
            "network-checker", // vendor connectivity watchdog
            "azure",
            "kelby",
            "garden",
        ];
        if VENDOR.iter().any(|needle| lower.contains(needle)) {
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
        if let Ok((temp, humidity)) = aht20() {
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

    /// AHT20 air temperature and humidity.
    ///
    /// This was an AM2320 driver until 2026-08-31, when it returned nothing on the real
    /// device while the factory happily reported 25.3387451171875 °C and
    /// 54.88100051879883 %. Those are not AM2320 numbers — that part reports tenths of a
    /// degree in 16 bits, and would have said 25.3. They are exactly 394992 and 575469
    /// over 2²⁰, which is the AHT conversion. `0x38` is the AHT family's address; an
    /// AM2320 lives at `0x5C`. See `super::aht20_convert` for the arithmetic, which is
    /// tested against those two readings.
    ///
    /// No calibration command is sent. The factory's `gy_events` is reading this part
    /// several times a minute, so it is demonstrably already initialised, and issuing
    /// `0xBE` mid-conversation would be a write to a device we do not own yet.
    fn aht20() -> Result<(f32, f32)> {
        let mut bus = open(expected::AHT20)?;

        bus.write(&[0xAC, 0x33, 0x00])
            .map_err(|e| HardwareError::Bus(format!("AHT20 trigger: {e}")))?;
        // The datasheet asks for 75 ms; the extra is cheap once a minute.
        std::thread::sleep(std::time::Duration::from_millis(85));

        let mut buffer = [0u8; 6];
        bus.read(&mut buffer)
            .map_err(|e| HardwareError::Bus(format!("AHT20 read: {e}")))?;

        super::aht20_convert(&buffer)
            .ok_or_else(|| HardwareError::Bus(format!("AHT20 status 0x{:02x}", buffer[0])))
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
        /// Confirmed from the factory source: `sensors/Pump.py` configures its own
        /// INA219 with `SHUNT_OHMS = 0.08`. The 100 mΩ assumed here previously was a
        /// guess, and it under-reported every current by a fifth.
        const SHUNT_MILLIOHMS: f32 = 80.0;

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
            // Worth shouting about, because the symptom otherwise is simply never being
            // told to add water.
            //
            // `Invalid argument (os error 22)` on the echo interrupt is not a
            // permissions problem, whatever it looks like. rppal 0.19 drives GPIO
            // interrupts through the `GPIO_V2_*` character-device ioctls, and that uAPI
            // arrived in Linux 5.10. This Gardyn runs 4.14, which has only the v1
            // interface, and an unrecognised ioctl number is EINVAL. Sending someone to
            // check `id -nG` when they are already in `gpio` wastes the one clue.
            Err(error) => {
                let kernel_too_old = error.to_string().contains("os error 22");
                tracing::warn!(
                    %error,
                    "cannot open the ultrasonic sensor — water level will be missing, and \
                     the water rule cannot run without it"
                );
                if kernel_too_old {
                    tracing::warn!(
                        "EINVAL from the echo interrupt means the kernel is older than \
                         the GPIO v2 uAPI rppal needs (5.10; this device runs 4.14). Not \
                         a permissions problem. Until Phase 6 the vendor's `gy_wl` owns \
                         this sensor anyway — read its value from \
                         /usr/local/etc/sensors/water_lvl_status instead of contending \
                         for the trigger pin. See DESIGN.md §6."
                    );
                } else {
                    tracing::warn!("check the agent's user is in the `gpio` group (`id -nG`)");
                }
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
        // Our own sensor first, the factory's file second. The order matters at Phase 6:
        // once the vendor stack is gone its file stops being written, and this falls
        // back to the measurement we will by then be able to take ourselves.
        if snapshot.water_level_mm.is_none() {
            snapshot.water_level_mm = crate::vendor::water_level()
                .ok()
                .and_then(|level| level.distance_mm());
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
mod aht20_tests {
    use super::aht20_convert;

    /// Encode a temperature and humidity the way the part would, so a test can be
    /// written in degrees rather than in hex.
    fn frame(status: u8, humidity_raw: u32, temp_raw: u32) -> [u8; 6] {
        [
            status,
            (humidity_raw >> 12) as u8,
            (humidity_raw >> 4) as u8,
            (((humidity_raw & 0x0F) << 4) | (temp_raw >> 16)) as u8,
            (temp_raw >> 8) as u8,
            temp_raw as u8,
        ]
    }

    #[test]
    fn it_decodes_the_readings_the_factory_reported() {
        // The whole reason this is an AHT20 driver and not an AM2320 one. `gy_events`
        // wrote 25.3387451171875 °C and 54.88100051879883 % to its status files while
        // our AM2320 read returned nothing; those are 394992 and 575469 over 2²⁰.
        let (temp, humidity) = aht20_convert(&frame(0x1C, 575_469, 394_992)).unwrap();
        assert!((temp - 25.338_745).abs() < 1e-3, "{temp}");
        assert!((humidity - 54.881).abs() < 1e-3, "{humidity}");
    }

    #[test]
    fn the_two_fields_do_not_bleed_into_each_other() {
        // They share byte 3, a nibble each, which is the easy thing to get wrong. Each
        // case drives one field to an extreme while the other sits at a plain value, so
        // a leaking nibble moves the answer by tens of degrees rather than subtly.
        let (temp, humidity) = aht20_convert(&frame(0x1C, 0xF_FFFF, 262_144)).unwrap();
        assert!((humidity - 100.0).abs() < 0.01, "{humidity}");
        assert!(temp.abs() < 0.01, "{temp} should be 0 °C");

        let (temp, humidity) = aht20_convert(&frame(0x1C, 0, 655_360)).unwrap();
        assert!(humidity.abs() < 0.01, "{humidity}");
        assert!((temp - 75.0).abs() < 0.01, "{temp} should be 75 °C");
    }

    #[test]
    fn a_busy_conversion_is_no_reading_rather_than_a_stale_one() {
        // Bit 7 means the measurement is still running and the buffer holds the
        // previous one. We share this part with the factory's reader, so it happens.
        assert!(aht20_convert(&frame(0x80, 575_469, 394_992)).is_none());
    }

    #[test]
    fn an_absent_part_is_rejected_rather_than_reported_as_a_cold_room() {
        // All zeros and all ones are what a missing or uncalibrated part looks like,
        // and both convert to numbers a rule would happily act on.
        assert!(aht20_convert(&[0; 6]).is_none());
        assert!(aht20_convert(&frame(0x1C, 0xF_FFFF, 0xF_FFFF)).is_none());
    }
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
                report
                    .warnings
                    .iter()
                    .any(|w| w.contains("development machine")),
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
