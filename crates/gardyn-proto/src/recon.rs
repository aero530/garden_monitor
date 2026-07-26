//! Phase 0 reconnaissance: what is actually inside this device.
//!
//! Everything the peripheral map in DESIGN.md claims comes from community work on the
//! Gardyn Home 3.0/4.0. Studio 2 internals are undocumented, so the first thing the
//! agent does on real hardware is describe what it finds rather than assume.
//!
//! A serialisable report so it can be committed to the repo next to DESIGN.md and
//! diffed after a firmware update.

use serde::{Deserialize, Serialize};

/// The peripherals DESIGN.md expects, from `iot-root/garden-of-eden`.
pub mod expected {
    /// AM2320 air temperature and humidity.
    pub const AM2320: u16 = 0x38;
    /// INA219 pump current monitor.
    pub const INA219: u16 = 0x40;
    /// PCT2075 board temperature.
    pub const PCT2075: u16 = 0x48;
    /// ADS1115 for analogue EC/pH, once fitted. Defaults to 0x48 and **collides**
    /// with the PCT2075, so it must be strapped to 0x49.
    pub const ADS1115_STRAPPED: u16 = 0x49;

    pub const ALL_I2C: &[(u16, &str)] = &[
        (AM2320, "AM2320 air temp/humidity"),
        (INA219, "INA219 pump current"),
        (PCT2075, "PCT2075 board temp"),
    ];

    /// Hardware PWM driving the LED bars.
    pub const GPIO_LIGHT: u8 = 18;
    /// Hardware PWM driving the pump.
    pub const GPIO_PUMP: u8 = 24;
    /// Ultrasonic water level, trigger and echo.
    pub const GPIO_ULTRASONIC_TRIG: u8 = 19;
    pub const GPIO_ULTRASONIC_ECHO: u8 = 26;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct I2cDevice {
    pub address: u16,
    /// What DESIGN.md expects at this address, if anything.
    pub expected: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraDevice {
    pub path: String,
    pub name: Option<String>,
    pub formats: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReconReport {
    pub agent_version: String,
    pub captured_at: String,

    // --- Board and OS -------------------------------------------------------------
    /// `/proc/device-tree/model`, e.g. "Raspberry Pi Zero 2 W Rev 1.0".
    pub board_model: Option<String>,
    pub cpu_architecture: Option<String>,
    /// `PRETTY_NAME` from `/etc/os-release`.
    pub os: Option<String>,
    pub kernel: Option<String>,

    // --- Peripherals --------------------------------------------------------------
    pub i2c_bus: Option<String>,
    pub i2c_devices: Vec<I2cDevice>,
    /// Addresses DESIGN.md expects that did not answer.
    pub i2c_missing: Vec<String>,
    pub cameras: Vec<CameraDevice>,
    /// 1-Wire device ids, where a DS18B20 water probe would appear.
    pub one_wire_devices: Vec<String>,
    /// Whether `/sys/class/pwm` is exported, and by whom.
    pub pwm_channels: Vec<String>,

    // --- What else is running -------------------------------------------------------
    /// Services that look like Gardyn's own software.
    pub vendor_services: Vec<String>,
    /// True when `pigpiod` is running, which is how the factory firmware is expected
    /// to drive PWM and how we read its duty cycle without a logic analyser.
    pub pigpiod_running: bool,

    /// Anything that could not be determined, with the reason.
    pub warnings: Vec<String>,
}

impl ReconReport {
    /// Whether the device matches the peripheral map DESIGN.md is built on.
    pub fn matches_expected_peripherals(&self) -> bool {
        self.i2c_missing.is_empty() && !self.i2c_devices.is_empty()
    }

    /// A one-line verdict for the console.
    pub fn verdict(&self) -> String {
        if self.i2c_devices.is_empty() {
            return "no I²C devices answered — check the bus is enabled and the ribbon \
                    is seated"
                .into();
        }
        if self.i2c_missing.is_empty() {
            "peripheral map matches DESIGN.md".into()
        } else {
            format!(
                "{} expected device(s) missing: {} — DESIGN.md §2 needs updating for \
                 this board",
                self.i2c_missing.len(),
                self.i2c_missing.join(", ")
            )
        }
    }

    /// Whether a DS18B20 water-temperature probe is present.
    pub fn has_water_probe(&self) -> bool {
        // 1-Wire temperature sensors enumerate with a `28-` family prefix.
        self.one_wire_devices.iter().any(|d| d.starts_with("28-"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_with(addresses: &[u16]) -> ReconReport {
        let mut report = ReconReport::default();
        for (address, name) in expected::ALL_I2C {
            if addresses.contains(address) {
                report.i2c_devices.push(I2cDevice {
                    address: *address,
                    expected: Some((*name).to_string()),
                });
            } else {
                report.i2c_missing.push(format!("0x{address:02x} {name}"));
            }
        }
        report
    }

    #[test]
    fn a_matching_board_says_so() {
        let report = report_with(&[expected::AM2320, expected::INA219, expected::PCT2075]);
        assert!(report.matches_expected_peripherals());
        assert_eq!(report.verdict(), "peripheral map matches DESIGN.md");
    }

    #[test]
    fn a_missing_peripheral_names_itself() {
        let report = report_with(&[expected::AM2320, expected::INA219]);
        assert!(!report.matches_expected_peripherals());
        assert!(report.verdict().contains("0x48"), "{}", report.verdict());
        assert!(report.verdict().contains("DESIGN.md"));
    }

    #[test]
    fn an_empty_bus_is_reported_as_a_wiring_problem_not_a_hardware_difference() {
        // The likely cause of a silent bus is a disabled interface, and saying
        // "everything is missing" would send someone rewriting the design doc.
        let report = ReconReport::default();
        assert!(report.verdict().contains("check the bus"));
    }

    #[test]
    fn the_water_probe_is_detected_by_its_family_code() {
        let mut report = ReconReport::default();
        assert!(!report.has_water_probe());
        report.one_wire_devices.push("28-0000063a1b2c".into());
        assert!(report.has_water_probe());
    }

    #[test]
    fn a_non_temperature_one_wire_device_is_not_mistaken_for_the_probe() {
        let mut report = ReconReport::default();
        report.one_wire_devices.push("01-000000000000".into());
        assert!(!report.has_water_probe());
    }

    #[test]
    fn the_adc_address_avoids_the_board_temperature_sensor() {
        // The collision that would otherwise be discovered by two sensors returning
        // nonsense at the same address.
        assert_ne!(expected::ADS1115_STRAPPED, expected::PCT2075);
    }

    #[test]
    fn a_report_round_trips_so_it_can_be_committed_and_diffed() {
        let report = report_with(&[expected::AM2320]);
        let json = serde_json::to_string_pretty(&report).unwrap();
        let back: ReconReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
    }
}
