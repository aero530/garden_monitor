//! Reading the factory firmware's own sensor files.
//!
//! **This module is temporary by construction.** At Phase 6 the vendor stack is gone
//! and so is everything here. Until then it is the contention-free way to see what the
//! device already knows about itself.
//!
//! The tank is the case that matters. `garden-edge` cannot measure it directly on this
//! device for two reasons, and the second is the important one:
//!
//! 1. `rppal` 0.19 registers GPIO interrupts through the `GPIO_V2_*` character-device
//!    ioctls, and that uAPI arrived in Linux 5.10. Raspbian 9 ships 4.14, where an
//!    unrecognised ioctl is `EINVAL`.
//! 2. `gy_wl` owns the trigger pin and pulses it every ten seconds. Even on a modern
//!    kernel, us pulsing it too would be two processes driving one output — the exact
//!    conflict Phase 1 exists to avoid.
//!
//! So we read their answer rather than racing them for the sensor. `gy_wl` writes a
//! plain number to a file; taking it costs one `read` and disturbs nothing.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Where `gy_wl` keeps the current tank reading. `config.py`, `WATER_LVL_STATUS`.
const WATER_LEVEL_PATH: &str = "/usr/local/etc/sensors/water_lvl_status";

/// How old the file may be **once nothing is maintaining it**.
///
/// This started at two minutes, on the reasoning that `main.py`'s `thread_water_level`
/// sleeps ten seconds between samples. That was wrong twice over. That thread does not
/// run on this device — `services_enabled` is `waterlevel,iot`, so
/// `isWaterlevelServiceEnabled()` is true and `gy_wl` is the writer instead. And `gy_wl`
/// appears to write on *change* rather than on a timer: its `-n 20 -t 10` matches
/// `WL_DATAPOINTS_SIZE = 20`, and `WL_DATAPOINTS_DELTA_CHANGE_UP`/`DOWN` are 2–3 cm. A
/// tank that has not moved 3 cm is not written for hours, and the first real reading on
/// this device was a perfectly good 28 minutes old.
///
/// So age alone cannot tell a healthy quiet tank from a dead writer. The liveness
/// question is asked of the process instead, and this bound only applies once the
/// answer is no — at which point the file can only get staler.
const MAX_AGE_WITHOUT_WRITER: Duration = Duration::from_secs(15 * 60);

/// The process that maintains the file. `systemctl list-units` calls it `gy_wl.service`.
const WRITER_PROCESS: &str = "gy_wl";

/// The factory's own sanity band, `WL_DATAPOINTS_CLAMP_MIN`/`MAX` in `config.py`,
/// in centimetres. Their reader already clamps to this, so a value outside it means we
/// are not reading what we think we are.
const CLAMP_MIN_CM: f32 = 3.0;
const CLAMP_MAX_CM: f32 = 25.0;

/// **Confirmed 2026-08-31**: the factory's number is the distance from the sensor down
/// to the water, not the depth of the water.
///
/// Water was added to the tank and the reading fell from 67.34 mm to 56.0 mm. Only a
/// distance does that — a depth would have risen. So this matches what `garden-core`
/// means by `water_level_mm`, small when full and large when empty, and the conversion
/// is just centimetres to millimetres.
///
/// The factory source could not settle it: `config.py` calls the value a "water level",
/// and the two places in `main.py` that map a change onto "up" or "down" disagree with
/// each other about the sign. A jug of water settled it in one reading. Kept as a named
/// constant because the evidence is a measurement someone made once, not something the
/// code can re-derive.
const VALUE_IS_DISTANCE_TO_SURFACE: bool = true;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum VendorError {
    #[error("{WATER_LEVEL_PATH} does not exist — the factory stack is not running here")]
    Absent,
    #[error(
        "gy_wl is not running and last wrote {}s ago; nothing is maintaining this value",
        age.as_secs()
    )]
    Stale { age: Duration },
    /// `main.py` writes the literal string `error` when its own read throws.
    #[error("gy_wl is reporting a sensor error")]
    SensorError,
    #[error("cannot read a number from {WATER_LEVEL_PATH}: {0:?}")]
    Unparseable(String),
    #[error("{0} cm is outside the factory's own {CLAMP_MIN_CM}–{CLAMP_MAX_CM} cm band")]
    OutOfBand(f32),
}

/// The factory's tank reading, and enough context to judge it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaterLevel {
    /// Exactly what was in the file, before any interpretation.
    pub raw_cm: f32,
    /// How long ago `gy_wl` wrote it. Large is normal — see
    /// [`MAX_AGE_WITHOUT_WRITER`] — but worth printing, because a wedged writer is
    /// still running while its file goes quietly out of date, and that is the one
    /// failure this design cannot detect on its own.
    pub age: Duration,
    /// Whether `gy_wl` is still there to update it.
    pub writer_running: bool,
}

impl WaterLevel {
    /// As `garden-core` means it: distance from the sensor to the surface.
    ///
    /// `None` when [`VALUE_IS_DISTANCE_TO_SURFACE`] is false, because turning a depth
    /// into a distance needs the tank geometry, and this device's geometry is still the
    /// placeholder in `TankGeometry::STUDIO_2`. Reporting nothing beats reporting a
    /// number derived from two guesses at once.
    pub fn distance_mm(self) -> Option<f32> {
        VALUE_IS_DISTANCE_TO_SURFACE.then_some(self.raw_cm * 10.0)
    }
}

/// Read the factory's current tank level.
pub fn water_level() -> Result<WaterLevel, VendorError> {
    read_water_level(Path::new(WATER_LEVEL_PATH), SystemTime::now(), writer_running())
}

/// Whether `gy_wl` is still there to maintain the file.
///
/// This is the liveness question, and it is asked of the process rather than of the
/// file's timestamp because the file is only written when the tank moves.
fn writer_running() -> bool {
    std::process::Command::new("pgrep")
        .args(["-x", WRITER_PROCESS])
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Split out so the parsing and the staleness rule are testable without the device.
fn read_water_level(
    path: &Path,
    now: SystemTime,
    writer_running: bool,
) -> Result<WaterLevel, VendorError> {
    let text = std::fs::read_to_string(path).map_err(|_| VendorError::Absent)?;
    let trimmed = text.trim();

    if trimmed.eq_ignore_ascii_case("error") {
        return Err(VendorError::SensorError);
    }

    // A file that has never been written is empty rather than absent, and parsing that
    // as an error message would be confusing.
    let raw_cm: f32 = trimmed
        .parse()
        .map_err(|_| VendorError::Unparseable(trimmed.chars().take(40).collect()))?;
    if !raw_cm.is_finite() {
        return Err(VendorError::Unparseable(trimmed.to_string()));
    }
    if !(CLAMP_MIN_CM..=CLAMP_MAX_CM).contains(&raw_cm) {
        return Err(VendorError::OutOfBand(raw_cm));
    }

    // Staleness last: a value that is out of band is wrong whether it is fresh or not,
    // and saying so is more useful than saying it is old.
    let age = path
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|written| now.duration_since(written).ok())
        .unwrap_or_default();
    // Age is only damning once nothing is left to update the file. While `gy_wl` runs,
    // an old timestamp means the tank has not moved far enough to be worth writing.
    if !writer_running && age > MAX_AGE_WITHOUT_WRITER {
        return Err(VendorError::Stale { age });
    }

    Ok(WaterLevel {
        raw_cm,
        age,
        writer_running,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A scratch file per call.
    ///
    /// The pid and a counter, not just `name`: two `cargo test` invocations overlapping
    /// would otherwise share these paths and one would truncate the other's fixture
    /// mid-read. Cheap insurance against a failure that looks like a logic bug.
    fn write_temp(name: &str, contents: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let unique = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "garden-vendor-{name}-{}-{unique}",
            std::process::id()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        path
    }

    const RUNNING: bool = true;
    const GONE: bool = false;

    #[test]
    fn it_reads_the_number_the_factory_wrote() {
        let path = write_temp("ok", "12.5\n");
        let level = read_water_level(&path, SystemTime::now(), RUNNING).unwrap();
        assert_eq!(level.raw_cm, 12.5);
        assert_eq!(level.distance_mm(), Some(125.0));
    }

    #[test]
    fn a_quiet_tank_is_not_mistaken_for_a_dead_writer() {
        // The bug this file shipped with. The first real reading off the device was 28
        // minutes old and perfectly good — `gy_wl` writes when the tank moves 2–3 cm,
        // not on a timer — and a two-minute freshness rule threw it away.
        let path = write_temp("quiet", "12.5");
        let half_an_hour_on = SystemTime::now() + Duration::from_secs(1710);
        let level = read_water_level(&path, half_an_hour_on, RUNNING).unwrap();
        assert_eq!(level.raw_cm, 12.5);
        assert!(level.age.as_secs() >= 1700);
        assert!(level.writer_running);
    }

    #[test]
    fn a_stopped_gy_wl_is_an_error_rather_than_an_old_number() {
        // Still the dangerous case, just diagnosed by asking after the process rather
        // than the timestamp: nothing is left to update the file, so a forecast would
        // extrapolate from a reading that can only get older.
        let path = write_temp("stale", "12.5");
        let much_later = SystemTime::now() + Duration::from_secs(3600);
        assert!(matches!(
            read_water_level(&path, much_later, GONE),
            Err(VendorError::Stale { .. })
        ));

        // A writer that has only just died still has a usable value.
        assert!(read_water_level(&path, SystemTime::now(), GONE).is_ok());
    }

    #[test]
    fn the_vendors_own_error_string_is_recognised() {
        // `main.py` sets water_lvl = "error" when its read throws, so this is a string
        // that really does turn up in that file.
        let path = write_temp("err", "error");
        assert_eq!(
            read_water_level(&path, SystemTime::now(), RUNNING),
            Err(VendorError::SensorError)
        );
    }

    #[test]
    fn a_value_outside_the_factorys_own_band_is_refused() {
        // Their reader clamps to 3–25 cm, so anything else means we are reading a
        // different file, a different unit, or a different firmware.
        for contents in ["0.5", "97", "-4"] {
            let path = write_temp("band", contents);
            assert!(
                matches!(
                    read_water_level(&path, SystemTime::now(), RUNNING),
                    Err(VendorError::OutOfBand(_))
                ),
                "{contents} should be out of band"
            );
        }
    }

    #[test]
    fn an_empty_or_absent_file_says_which() {
        let path = write_temp("empty", "");
        assert!(matches!(
            read_water_level(&path, SystemTime::now(), RUNNING),
            Err(VendorError::Unparseable(_))
        ));
        assert_eq!(
            read_water_level(
                Path::new("/nonexistent/water_lvl_status"),
                SystemTime::now(),
                RUNNING
            ),
            Err(VendorError::Absent)
        );
    }
}
