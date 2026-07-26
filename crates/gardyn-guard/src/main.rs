//! Failsafe supervisor.
//!
//! Layer 3 of the safety model in DESIGN.md, and only relevant after Phase 6 hands us
//! the actuators. `gardyn-edge` writes a heartbeat file; if it stops for long enough,
//! this seizes the PWM lines and runs a conservative schedule.
//!
//! Deliberately a separate process with almost no dependencies. The whole point is
//! that a panic in the complicated program cannot take out the simple one — so this
//! has no HTTP client, no database, no async runtime, and no rules engine. It reads a
//! file's mtime and drives two pins.
//!
//! **Not wired to real hardware yet**, because nothing owns the pins until Phase 6.
//! Until then it runs, watches, and logs what it *would* do, which is exactly what you
//! want while proving the watchdog before trusting it with a crop.

use clap::Parser;
use gardyn_hal::Duty;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Light hours per day the failsafe runs.
///
/// Chosen to be adequate for everything in Gardyn's catalogue rather than optimal for
/// anything. A failsafe should keep plants alive until someone notices, not grow them
/// well.
const FAILSAFE_LIGHT_HOURS: f64 = 14.0;
/// Pump: minutes on, then off, repeating.
const FAILSAFE_PUMP_ON_MINUTES: f64 = 15.0;
const FAILSAFE_PUMP_CYCLE_MINUTES: f64 = 60.0;
/// Duty while the pump runs. Well under the 30% ceiling the supply can take.
const FAILSAFE_PUMP_DUTY: f32 = 0.25;
/// Light duty while on.
const FAILSAFE_LIGHT_DUTY: f32 = 0.80;

#[derive(Parser)]
#[command(name = "gardyn-guard", version, about = "Gardyn failsafe supervisor")]
struct Cli {
    /// Heartbeat file the edge agent touches.
    #[arg(long, env = "GARDYN_HEARTBEAT", default_value = "/run/gardyn/edge.heartbeat")]
    heartbeat: PathBuf,

    /// Seconds of silence before the agent is presumed dead.
    ///
    /// Generous by default: a brief stall during a frame upload must not cause the
    /// guard to fight the agent for the pins.
    #[arg(long, env = "GARDYN_GRACE_SECONDS", default_value_t = 300)]
    grace_seconds: u64,

    /// How often to check.
    #[arg(long, default_value_t = 15)]
    interval_seconds: u64,

    /// Log what would happen without touching any pin. Default until Phase 6.
    #[arg(long, env = "GARDYN_GUARD_DRY_RUN", default_value_t = true)]
    dry_run: bool,
}

/// What the failsafe schedule wants at a given moment.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Setpoint {
    light: Duty,
    pump: Duty,
}

/// The conservative schedule, as a pure function of elapsed time.
///
/// Takes seconds-since-midnight rather than a clock so it is trivially testable, and
/// so a wrong timezone cannot make the lights come on at 3am.
fn failsafe_setpoint(seconds_since_midnight: u64) -> Setpoint {
    let hour = (seconds_since_midnight as f64) / 3600.0;
    let light = if hour < FAILSAFE_LIGHT_HOURS {
        Duty::new(FAILSAFE_LIGHT_DUTY)
    } else {
        Duty::OFF
    };

    let minute_in_cycle = ((seconds_since_midnight as f64) / 60.0) % FAILSAFE_PUMP_CYCLE_MINUTES;
    let pump = if minute_in_cycle < FAILSAFE_PUMP_ON_MINUTES {
        // `Duty::pump` clamps to the current ceiling regardless of what is asked for,
        // which is the invariant that protects the supply.
        Duty::pump(FAILSAFE_PUMP_DUTY)
    } else {
        Duty::OFF
    };

    Setpoint { light, pump }
}

/// Seconds since the heartbeat file was last touched.
///
/// A missing file counts as infinitely stale: if the agent has never run, the garden
/// still needs light and water.
fn heartbeat_age(path: &PathBuf) -> Option<Duration> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gardyn_guard=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let grace = Duration::from_secs(cli.grace_seconds);
    let interval = Duration::from_secs(cli.interval_seconds.max(1));

    tracing::info!(
        heartbeat = %cli.heartbeat.display(),
        grace_seconds = cli.grace_seconds,
        dry_run = cli.dry_run,
        "failsafe supervisor started"
    );
    if cli.dry_run {
        tracing::info!(
            "dry run: the guard will log what it would do but will not touch a pin. \
             Clear GARDYN_GUARD_DRY_RUN only after Phase 6."
        );
    }

    let mut engaged = false;
    loop {
        let age = heartbeat_age(&cli.heartbeat);
        let stale = age.map(|a| a > grace).unwrap_or(true);

        if stale && !engaged {
            engaged = true;
            tracing::error!(
                age_seconds = age.map(|a| a.as_secs()),
                "edge agent is not beating — engaging the failsafe schedule"
            );
        } else if !stale && engaged {
            engaged = false;
            tracing::info!("edge agent is back — standing down");
        }

        if engaged {
            let seconds = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs() % 86_400)
                .unwrap_or(0);
            let setpoint = failsafe_setpoint(seconds);
            if cli.dry_run {
                tracing::info!(
                    light = setpoint.light.percent(),
                    pump = setpoint.pump.percent(),
                    "would drive"
                );
            } else {
                // Phase 6 wires this to rppal. Until the takeover happens the factory
                // firmware owns the pins and writing to them would be a fight.
                tracing::warn!("actuator control is not implemented until Phase 6");
            }
        }

        std::thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3600;

    #[test]
    fn the_lights_run_fourteen_hours_and_then_stop() {
        assert!(!failsafe_setpoint(0).light.is_off());
        assert!(!failsafe_setpoint(13 * HOUR).light.is_off());
        assert!(failsafe_setpoint(15 * HOUR).light.is_off());
        assert!(failsafe_setpoint(23 * HOUR).light.is_off());
    }

    #[test]
    fn the_pump_cycles_fifteen_minutes_in_every_hour() {
        assert!(!failsafe_setpoint(0).pump.is_off());
        assert!(!failsafe_setpoint(14 * 60).pump.is_off());
        assert!(failsafe_setpoint(16 * 60).pump.is_off());
        assert!(failsafe_setpoint(59 * 60).pump.is_off());
        // ...and starts again on the next hour.
        assert!(!failsafe_setpoint(HOUR).pump.is_off());
    }

    #[test]
    fn the_pump_keeps_running_through_the_dark_hours() {
        // Roots do not stop needing water when the lights go off. A failsafe that
        // tied the two together would dry the tower out overnight.
        let night = failsafe_setpoint(20 * HOUR);
        assert!(night.light.is_off());
        assert!(!night.pump.is_off());
    }

    #[test]
    fn the_pump_never_exceeds_its_current_ceiling() {
        for seconds in (0..86_400).step_by(300) {
            let setpoint = failsafe_setpoint(seconds);
            assert!(
                setpoint.pump.get() <= Duty::PUMP_MAX,
                "pump at {} exceeded the ceiling",
                setpoint.pump.get()
            );
        }
    }

    #[test]
    fn the_schedule_is_defined_for_every_second_of_the_day() {
        for seconds in (0..86_400).step_by(97) {
            let setpoint = failsafe_setpoint(seconds);
            assert!((0.0..=1.0).contains(&setpoint.light.get()));
            assert!((0.0..=1.0).contains(&setpoint.pump.get()));
        }
    }

    #[test]
    fn a_missing_heartbeat_file_counts_as_stale() {
        // The case that matters most: the agent has never started, and the garden
        // still needs light and water.
        let absent = PathBuf::from("/definitely/not/a/real/heartbeat");
        assert!(heartbeat_age(&absent).is_none());
    }

    #[test]
    fn a_fresh_heartbeat_file_is_not_stale() {
        let path = std::env::temp_dir().join(format!(
            "gardyn-guard-test-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"beat").unwrap();
        let age = heartbeat_age(&path).expect("a file we just wrote has an mtime");
        assert!(age < Duration::from_secs(5));
        let _ = std::fs::remove_file(&path);
    }
}
