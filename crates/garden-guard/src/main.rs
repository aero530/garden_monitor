//! Failsafe supervisor.
//!
//! Layer 3 of the safety model in DESIGN.md, and only relevant after Phase 6 hands us
//! the actuators. `garden-edge` writes a heartbeat file; if it stops for long enough,
//! this seizes the PWM lines and runs a conservative schedule.
//!
//! Deliberately a separate process with almost no dependencies. The whole point is
//! that a panic in the complicated program cannot take out the simple one — so this
//! has no HTTP client, no database, no async runtime, and no rules engine. It reads a
//! file's mtime and drives two pins.
//!
//! **Dry-run by default**, because until Phase 6 the factory firmware owns the pins
//! and two processes fighting over a PWM line is how you lose a crop. In dry-run it
//! still runs the whole loop and logs what it would have driven, which is what the
//! weeks of watching before you trust a watchdog actually look like.

mod pins;

use clap::Parser;
use garden_hal::{GuardMarker, Heartbeat, Schedule, Setpoint};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[derive(Parser)]
#[command(name = "garden-guard", version, about = "Garden failsafe supervisor")]
struct Cli {
    /// Heartbeat file the edge agent touches.
    #[arg(long, env = "GARDEN_HEARTBEAT", default_value = "/run/garden/edge.heartbeat")]
    heartbeat: PathBuf,

    /// Seconds of silence before the agent is presumed dead.
    ///
    /// Generous by default: a brief stall during a frame upload must not cause the
    /// guard to fight the agent for the pins.
    #[arg(long, env = "GARDEN_GRACE_SECONDS", default_value_t = 300)]
    grace_seconds: u64,

    /// How often to check.
    #[arg(long, default_value_t = 15)]
    interval_seconds: u64,

    /// Log what would happen without touching any pin. Default until Phase 6.
    #[arg(long, env = "GARDEN_GUARD_DRY_RUN", default_value_t = true)]
    dry_run: bool,

    /// The file this writes to tell the agent it has taken the pins.
    ///
    /// The agent watches it and stands down. Without this the two would drive the same
    /// line whenever a slow tick made the agent look briefly dead.
    #[arg(
        long,
        env = "GARDEN_GUARD_MARKER",
        default_value = "/run/garden/guard.engaged"
    )]
    marker: PathBuf,
}

/// The conservative schedule at a given moment.
///
/// Takes seconds-since-midnight rather than a clock so the whole day is trivially
/// testable, and so a wrong timezone cannot make the lights come on at 3am without a
/// test noticing.
fn failsafe_setpoint(seconds_since_midnight: u64) -> Setpoint {
    Schedule::FAILSAFE.setpoint((seconds_since_midnight % 86_400) as u32)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "garden_guard=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let grace = Duration::from_secs(cli.grace_seconds);
    let interval = Duration::from_secs(cli.interval_seconds.max(1));
    let heartbeat = Heartbeat::new(&cli.heartbeat);
    let marker = GuardMarker::new(&cli.marker);
    let mut output = pins::Output::open(cli.dry_run);

    tracing::info!(
        heartbeat = %heartbeat.path().display(),
        marker = %marker.path().display(),
        grace_seconds = cli.grace_seconds,
        live = output.is_live(),
        "failsafe supervisor started"
    );
    if !output.is_live() {
        tracing::info!(
            "dry run: the guard will log what it would do but will not touch a pin. \
             Clear GARDEN_GUARD_DRY_RUN only after Phase 6."
        );
    }

    // A marker left behind by a crashed guard would keep the agent standing down for
    // ever, so the pins are explicitly disowned at start-up. Safe because we have not
    // engaged yet: whatever is driving them now keeps driving them.
    if let Err(e) = marker.release() {
        tracing::warn!(%e, "could not clear a stale marker");
    }

    let mut engaged = false;
    loop {
        let age = heartbeat.age();
        let stale = heartbeat.is_stale(grace);

        if stale && !engaged {
            engaged = true;
            tracing::error!(
                age_seconds = age.map(|a| a.as_secs()),
                "edge agent is not beating — engaging the failsafe schedule"
            );
            // Claimed *before* the first write, so the agent has already stood down by
            // the time we touch a pin.
            if let Err(e) = marker.engage() {
                tracing::error!(%e, "could not claim the pins; the agent may still be driving");
            }
        } else if !stale && engaged {
            engaged = false;
            tracing::info!("edge agent is back — standing down");
            // Pump off first, then release: the reverse order would let the agent
            // resume while the failsafe still had the pump running.
            output.release_pump();
            if let Err(e) = marker.release() {
                tracing::error!(%e, "could not release the pins");
            }
        }

        if engaged {
            let seconds = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs() % 86_400)
                .unwrap_or(0);
            output.drive(failsafe_setpoint(seconds));
        }

        std::thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use garden_hal::Duty;

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

    /// Heartbeat staleness itself is covered in `garden_hal::handover`, which both
    /// processes share. What matters here is the transition it drives.
    #[test]
    fn the_marker_is_claimed_before_the_first_write_and_released_after_the_last() {
        // Ordering is the safety property: claim, then drive; stop the pump, then
        // release. Either reversed leaves a window where both processes own a pin.
        let path = std::env::temp_dir().join(format!(
            "garden-guard-handover-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let marker = GuardMarker::new(&path);
        let heartbeat = Heartbeat::new(path.with_extension("beat"));

        // Nothing has ever beaten, so the agent is presumed dead.
        assert!(heartbeat.is_stale(Duration::from_secs(300)));
        marker.engage().unwrap();
        assert!(marker.engaged(), "the agent must see this before we drive");

        let mut output = pins::Output::open(false);
        output.drive(failsafe_setpoint(6 * 3600));

        // The agent comes back.
        heartbeat.touch("0.1.0").unwrap();
        assert!(!heartbeat.is_stale(Duration::from_secs(300)));
        output.release_pump();
        marker.release().unwrap();
        assert!(!marker.engaged());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("beat"));
    }
}
