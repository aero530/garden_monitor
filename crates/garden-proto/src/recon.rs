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
    /// AHT20 air temperature and humidity.
    ///
    /// The Home 3.0/4.0 map calls this an AM2320, and `0x38` is where garden-of-eden
    /// found it — but an AM2320 answers at `0x5C` and reports tenths of a degree in 16
    /// bits. The factory's own readings on this unit are 25.3387451171875 °C and
    /// 54.88100051879883 %, which are exactly 394992 and 575469 over 2²⁰ through the
    /// AHT conversion. Twenty-bit, not sixteen, so the part is an AHT and the register
    /// protocol is a different one.
    pub const AHT20: u16 = 0x38;
    /// INA219 pump current monitor.
    pub const INA219: u16 = 0x40;
    /// PCT2075 board temperature.
    pub const PCT2075: u16 = 0x48;
    /// ADS1115 for analogue EC/pH, once fitted. Defaults to 0x48 and **collides**
    /// with the PCT2075, so it must be strapped to 0x49.
    pub const ADS1115_STRAPPED: u16 = 0x49;

    pub const ALL_I2C: &[(u16, &str)] = &[
        (AHT20, "AHT20 air temp/humidity"),
        (INA219, "INA219 pump current"),
        (PCT2075, "PCT2075 board temp"),
    ];

    /// Hardware PWM driving the LED bars.
    pub const GPIO_LIGHT: u8 = 18;
    /// Hardware PWM driving the pump.
    pub const GPIO_PUMP: u8 = 24;
    /// Ultrasonic water level, trigger and echo.
    ///
    /// **These are the opposite way round from the Home 3.0/4.0 map**, which had
    /// trigger on 19 and echo on 26. Phase 0 read GPIO19 as an input and GPIO26 as an
    /// output, and the factory source settles it: `config.py` declares
    /// `CHANNEL_WATER_LVL_IN = 19` and `CHANNEL_WATER_LVL_OUT = 26`. Getting this
    /// backwards is not a logic bug — it puts the Pi's output driver onto the pin the
    /// sensor is itself driving.
    pub const GPIO_ULTRASONIC_TRIG: u8 = 26;
    pub const GPIO_ULTRASONIC_ECHO: u8 = 19;

    /// GPIO0 and GPIO1 are the HAT ID EEPROM pins, `ID_SD` and `ID_SC`. On this board
    /// nothing uses them, so they read as plain inputs and look free — they are not.
    /// The firmware probes them at boot looking for an attached HAT, and they are the
    /// two lowest-numbered pins, so anything recommending "the first free GPIO" lands
    /// on exactly the pin it should not.
    pub const RESERVED_GPIOS: &[u8] = &[0, 1];

    /// Where `dtoverlay=w1-gpio` puts the 1-Wire bus unless given `gpiopin=`.
    pub const GPIO_ONE_WIRE_DEFAULT: u8 = 4;

    /// Push-to-toggle light button on the frame.
    pub const GPIO_BUTTON: u8 = 13;
    /// PCT2075 over-temperature interrupt. The factory trips it at 70 °C.
    pub const GPIO_PCB_TEMP_INTERRUPT: u8 = 25;

    /// The light PWM as the factory drives it: `hardware_PWM(18, 20000, percent *
    /// 10000)`, so brightness is a plain linear percentage of a 1,000,000 range.
    pub const LIGHT_PWM_HZ: u32 = 20_000;
    pub const LIGHT_PWM_RANGE: u32 = 1_000_000;

    /// What each documented pin should look like at rest, so the claim is checkable
    /// rather than merely written down. pigpio's `mg` reads the SoC's own
    /// function-select register, so this holds whoever configured the pin.
    pub const PIN_ROLES: &[(u8, &str, &str)] = &[
        (GPIO_LIGHT, "grow lights, hardware PWM", "alt5"),
        // Not PWM: the factory writes it high or low and leaves it there.
        (GPIO_PUMP, "pump, digital on/off", "output"),
        (GPIO_ULTRASONIC_TRIG, "ultrasonic trigger", "output"),
        (GPIO_ULTRASONIC_ECHO, "ultrasonic echo", "input"),
        (GPIO_BUTTON, "light button", "input"),
        (GPIO_PCB_TEMP_INTERRUPT, "PCB over-temperature interrupt", "input"),
    ];
}

/// One GPIO's function, as the SoC reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpioPin {
    pub gpio: u8,
    /// `input`, `output`, or `alt0`–`alt5`.
    pub mode: String,
    /// What DESIGN.md expects this pin to be doing, if anything.
    pub role: Option<String>,
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
    /// Every GPIO's function. Answers both halves of the pin question at once: whether
    /// the documented pins do what they are documented to do, and which pins are free
    /// for a DS18B20.
    #[serde(default)]
    pub gpio_modes: Vec<GpioPin>,

    // --- What else is running -------------------------------------------------------
    /// Services that look like Gardyn's own software.
    pub vendor_services: Vec<String>,
    /// True when `pigpiod` is running, which is how the factory firmware is expected
    /// to drive PWM and how we read its duty cycle without a logic analyser.
    pub pigpiod_running: bool,
    /// Which pigpio interface answered, if any — its socket, or its FIFOs.
    ///
    /// Separate from `pigpiod_running` because on the real unit the two disagreed: the
    /// service was up and `pigs` still answered `socket connect failed`, Raspbian
    /// having started the daemon as `pigpiod -l`. Only reachability predicts whether
    /// parity capture will record anything, so this is the field that gates Phase 1.
    #[serde(default)]
    pub pigpiod_interface: Option<String>,

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

    /// Documented pins whose actual function contradicts the role assigned to them.
    ///
    /// A trigger that reads as an input, or an echo that reads as an output, means the
    /// map is wrong — and acting on a wrong map is how you end up with the Pi and the
    /// sensor both driving the same wire.
    pub fn pin_role_conflicts(&self) -> Vec<String> {
        expected::PIN_ROLES
            .iter()
            .filter_map(|(gpio, role, wanted)| {
                let found = self.gpio_modes.iter().find(|p| p.gpio == *gpio)?;
                (found.mode != *wanted).then(|| {
                    format!(
                        "GPIO{gpio} is {} but DESIGN.md has it as {role}, which should \
                         read as {wanted}",
                        found.mode
                    )
                })
            })
            .collect()
    }

    /// Pins doing nothing, and so available for a DS18B20 or a parity jumper.
    pub fn free_gpios(&self) -> Vec<u8> {
        self.gpio_modes
            .iter()
            .filter(|p| p.mode == "input" && p.role.is_none())
            .map(|p| p.gpio)
            .filter(|gpio| !expected::RESERVED_GPIOS.contains(gpio))
            .collect()
    }

    /// Where to put the DS18B20.
    ///
    /// Prefers GPIO4, because that is where `dtoverlay=w1-gpio` puts the bus when not
    /// told otherwise — so taking it means one line in the boot config instead of one
    /// line plus a `gpiopin=` argument nobody will remember to check later.
    pub fn suggested_one_wire_gpio(&self) -> Option<u8> {
        let free = self.free_gpios();
        free.contains(&expected::GPIO_ONE_WIRE_DEFAULT)
            .then_some(expected::GPIO_ONE_WIRE_DEFAULT)
            .or_else(|| free.first().copied())
    }

    /// Whether the PWM pins can be read at all — the Phase 1 gate.
    ///
    /// `pigpiod_running` is the tempting thing to check and the wrong one.
    pub fn can_read_pwm(&self) -> bool {
        self.pigpiod_interface.is_some() || !self.pwm_channels.is_empty()
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

    fn pin(gpio: u8, mode: &str) -> GpioPin {
        GpioPin {
            gpio,
            mode: mode.to_string(),
            role: expected::PIN_ROLES
                .iter()
                .find(|(p, _, _)| *p == gpio)
                .map(|(_, role, _)| (*role).to_string()),
        }
    }

    #[test]
    fn the_pin_map_agrees_with_what_phase_0_read_off_the_device() {
        // Exactly the modes the Studio 2 reported on 2026-08-30. They contradicted the
        // Home 3.0/4.0 map on the ultrasonic pair — 19 is the echo, 26 the trigger, as
        // `config.py`'s CHANNEL_WATER_LVL_IN/OUT confirm — and this asserts the
        // correction stuck. An edit that swaps them back fails here, rather than on a
        // bench with two drivers on one wire.
        let report = ReconReport {
            gpio_modes: vec![
                pin(18, "alt5"),
                pin(24, "output"),
                pin(19, "input"),
                pin(26, "output"),
            ],
            ..Default::default()
        };
        assert!(
            report.pin_role_conflicts().is_empty(),
            "{:?}",
            report.pin_role_conflicts()
        );
        assert_eq!(expected::GPIO_ULTRASONIC_ECHO, 19);
        assert_eq!(expected::GPIO_ULTRASONIC_TRIG, 26);
    }

    #[test]
    fn a_trigger_that_reads_as_an_input_is_reported_as_a_conflict() {
        // The check has to still bite, or the test above only proves the constants
        // agree with themselves.
        let report = ReconReport {
            gpio_modes: vec![
                pin(expected::GPIO_ULTRASONIC_TRIG, "input"),
                pin(expected::GPIO_ULTRASONIC_ECHO, "output"),
            ],
            ..Default::default()
        };
        let conflicts = report.pin_role_conflicts();
        assert_eq!(conflicts.len(), 2, "{conflicts:?}");
        assert!(conflicts.iter().any(|c| c.contains("GPIO26")));
    }

    #[test]
    fn the_hat_id_pins_are_never_offered_as_free() {
        // GPIO0 and GPIO1 read as plain inputs on this board and are the two lowest
        // numbers, so "the first free pin" lands on the HAT ID EEPROM bus. The real
        // recon report from the device has both sitting there looking available.
        let report = ReconReport {
            gpio_modes: vec![pin(0, "input"), pin(1, "input"), pin(4, "input")],
            ..Default::default()
        };
        assert_eq!(report.free_gpios(), vec![4]);
        assert_eq!(report.suggested_one_wire_gpio(), Some(4));
    }

    #[test]
    fn the_water_probe_goes_on_the_overlays_default_pin_when_it_is_free() {
        // GPIO4 is what `dtoverlay=w1-gpio` uses with no arguments, so it is worth
        // preferring over a lower-numbered pin that would need `gpiopin=`.
        let report = ReconReport {
            gpio_modes: vec![pin(2, "input"), pin(4, "input"), pin(5, "input")],
            ..Default::default()
        };
        assert_eq!(report.suggested_one_wire_gpio(), Some(4));

        // And falls back rather than insisting, if something else has taken it.
        let taken = ReconReport {
            gpio_modes: vec![pin(4, "alt0"), pin(5, "input")],
            ..Default::default()
        };
        assert_eq!(taken.suggested_one_wire_gpio(), Some(5));
    }

    #[test]
    fn a_pin_with_a_documented_job_is_never_offered_as_free() {
        // GPIO19 reads as an input, but it is spoken for. Handing it to someone
        // looking for somewhere to put a DS18B20 would be a wiring bug.
        let report = ReconReport {
            gpio_modes: vec![pin(19, "input"), pin(5, "input"), pin(24, "output")],
            ..Default::default()
        };
        assert_eq!(report.free_gpios(), vec![5]);
    }

    #[test]
    fn an_unscanned_header_claims_neither_conflicts_nor_free_pins() {
        let report = ReconReport::default();
        assert!(report.pin_role_conflicts().is_empty());
        assert!(report.free_gpios().is_empty());
    }

    #[test]
    fn a_matching_board_says_so() {
        let report = report_with(&[expected::AHT20, expected::INA219, expected::PCT2075]);
        assert!(report.matches_expected_peripherals());
        assert_eq!(report.verdict(), "peripheral map matches DESIGN.md");
    }

    #[test]
    fn a_missing_peripheral_names_itself() {
        let report = report_with(&[expected::AHT20, expected::INA219]);
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
        let report = report_with(&[expected::AHT20]);
        let json = serde_json::to_string_pretty(&report).unwrap();
        let back: ReconReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
    }
}
