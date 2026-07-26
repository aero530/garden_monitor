//! The Gardyn edge agent.
//!
//! Runs on the device's Raspberry Pi. In Phase 1 it is strictly read-only: it reads
//! sensors, takes photographs, and reports to the brain, while the factory firmware
//! keeps running the lights and pump. Nothing here writes to an actuator, because two
//! processes contending for the same PWM pin is an excellent way to lose a tray of
//! seedlings.
//!
//! See HARDWARE.md for the full runbook.

mod actuators;
mod brain;
mod camera;
mod hardware;
mod pwm_watch;

use brain::{AGENT_VERSION, Client};
use clap::{Parser, Subcommand};
use gardyn_core::{GardenId, Timestamp};
use gardyn_proto::HeartbeatRequest;
use gardyn_hal::{Heartbeat, Schedule};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "gardyn-edge", version, about = "Gardyn device agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Brain base URL.
    #[arg(long, env = "GARDYN_BRAIN_URL", global = true, default_value = "http://localhost:8080")]
    brain_url: String,

    /// Shared agent token, matching GARDYN_AGENT_TOKEN on the brain.
    #[arg(long, env = "GARDYN_AGENT_TOKEN", global = true, default_value = "")]
    token: String,

    /// Which garden this device is.
    #[arg(long, env = "GARDYN_GARDEN_ID", global = true)]
    garden: Option<String>,

    /// Where unsent samples are buffered when the brain is unreachable.
    #[arg(long, env = "GARDYN_SPOOL_DIR", global = true, default_value = "/var/lib/gardyn/spool")]
    spool: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Phase 0: describe this device. Writes a JSON report and prints a summary.
    ///
    /// Needs no brain, no token and no garden id — run it first, on the device,
    /// before anything else.
    Probe {
        /// Where to write the report. Commit it next to DESIGN.md.
        #[arg(long, default_value = "recon-report.json")]
        out: PathBuf,
    },

    /// Read every sensor once and print the result. No network.
    Read,

    /// Read every sensor once and send it to the brain.
    Report,

    /// Take a photograph and upload it.
    Capture {
        /// Also write the image here.
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Parity capture: sample the factory firmware's PWM duty and log it to CSV.
    ///
    /// Run this for a week or two before Phase 6. It is the only record of what the
    /// stock light curve and pump cycle actually do, and it is gone the moment the
    /// vendor software is disabled.
    WatchPwm {
        #[arg(long, default_value = "pwm-parity.csv")]
        out: PathBuf,
        #[arg(long, default_value_t = 1)]
        interval_seconds: u64,
    },

    /// The daemon: register, then sample and photograph on a schedule.
    Run {
        /// **Phase 6.** Drive the light and pump pins from the resident schedule.
        ///
        /// Off by default and staying that way. Until the factory firmware is
        /// disabled it owns these pins, and two processes fighting over a PWM line is
        /// how you lose a crop. Turn this on only after `pwm-parity.csv` is recorded
        /// and `gardyn-guard` has been proven.
        #[arg(long, env = "GARDYN_OWN_ACTUATORS", default_value_t = false)]
        own_actuators: bool,

        /// Where `gardyn-guard` says it has seized the pins.
        #[arg(
            long,
            env = "GARDYN_GUARD_MARKER",
            default_value = "/run/gardyn/guard.engaged"
        )]
        guard_marker: PathBuf,

        /// File touched every tick, which is what tells the guard we are alive.
        #[arg(
            long,
            env = "GARDYN_HEARTBEAT",
            default_value = "/run/gardyn/edge.heartbeat"
        )]
        heartbeat: PathBuf,

        /// Seconds between sensor samples.
        #[arg(long, env = "GARDYN_SAMPLE_SECONDS", default_value_t = 60)]
        sample_seconds: u64,
        /// Seconds between photographs. Zero disables the camera.
        #[arg(long, env = "GARDYN_FRAME_SECONDS", default_value_t = 3600)]
        frame_seconds: u64,
        /// Name this device registers under on the fleet page.
        #[arg(long, env = "GARDYN_AGENT_NAME", default_value = "gardyn-edge")]
        name: String,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gardyn_edge=info".into()),
        )
        .init();

    let cli = Cli::parse();

    // `probe`, `read` and `watch-pwm` deliberately need no async runtime, no token and
    // no garden id. They are the commands you run on a device you have just opened,
    // possibly before the brain exists at all.
    match &cli.command {
        Command::Probe { out } => return probe(out),
        Command::Read => return read_once(),
        Command::WatchPwm {
            out,
            interval_seconds,
        } => return pwm_watch::run(out, Duration::from_secs((*interval_seconds).max(1))),
        _ => {}
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_main(cli))
}

fn probe(out: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let report = hardware::probe(AGENT_VERSION, Timestamp::now());
    let json = serde_json::to_string_pretty(&report)?;
    std::fs::write(out, &json)?;

    println!("Gardyn edge recon — agent {AGENT_VERSION}");
    println!();
    println!("  board    {}", report.board_model.as_deref().unwrap_or("unknown"));
    println!("  arch     {}", report.cpu_architecture.as_deref().unwrap_or("unknown"));
    println!("  os       {}", report.os.as_deref().unwrap_or("unknown"));
    println!("  kernel   {}", report.kernel.as_deref().unwrap_or("unknown"));
    println!();

    println!("  I²C devices:");
    if report.i2c_devices.is_empty() {
        println!("    none answered");
    }
    for device in &report.i2c_devices {
        match &device.expected {
            Some(name) => println!("    0x{:02x}  {name}", device.address),
            None => println!("    0x{:02x}  (not in the expected map)", device.address),
        }
    }

    println!("  cameras: {}", report.cameras.len());
    for camera in &report.cameras {
        println!("    {}", camera.path);
    }

    println!(
        "  water probe: {}",
        if report.has_water_probe() {
            "DS18B20 present"
        } else {
            "none"
        }
    );

    if !report.vendor_services.is_empty() {
        println!("  vendor services still running:");
        for service in &report.vendor_services {
            println!("    {service}");
        }
    }

    if !report.warnings.is_empty() {
        println!();
        println!("  warnings:");
        for warning in &report.warnings {
            println!("    - {warning}");
        }
    }

    println!();
    println!("  verdict: {}", report.verdict());
    println!();
    println!("Written to {}. Commit it next to DESIGN.md.", out.display());
    Ok(())
}

fn read_once() -> Result<(), Box<dyn std::error::Error>> {
    let snapshot = hardware::read_sensors(Timestamp::now());
    println!("{}", serde_json::to_string_pretty(&snapshot)?);
    let capabilities: Vec<_> = snapshot.capabilities().iter().map(|c| c.label()).collect();
    println!();
    println!("capabilities this reading demonstrates: {}", capabilities.join(", "));
    Ok(())
}

async fn async_main(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.token.is_empty() {
        return Err("GARDYN_AGENT_TOKEN is not set; the brain will reject every request".into());
    }
    let garden: GardenId = cli
        .garden
        .as_deref()
        .ok_or("GARDYN_GARDEN_ID is not set — create the garden in the web UI first")?
        .parse()
        .map_err(|_| "GARDYN_GARDEN_ID is not a valid id")?;

    let client = Client::new(&cli.brain_url, &cli.token, garden, cli.spool.clone())?;

    match cli.command {
        Command::Report => {
            let snapshot = hardware::read_sensors(Timestamp::now());
            let accepted = client.send_telemetry(&snapshot).await?;
            println!("accepted; brain sees: {}", accepted.capabilities.join(", "));
        }
        Command::Capture { out } => {
            let frame = camera::capture()?;
            if let Some(path) = &out {
                std::fs::write(path, &frame.bytes)?;
                println!("wrote {}", path.display());
            }
            client
                .upload_frame(
                    frame.bytes,
                    frame.captured_at,
                    frame.width,
                    frame.height,
                    None,
                    // Phase 1 cannot pin the lights — the factory firmware owns them —
                    // so frames are ambient and their colour is not comparable.
                    false,
                )
                .await?;
            println!("uploaded");
        }
        Command::Run {
            sample_seconds,
            frame_seconds,
            name,
            own_actuators,
            guard_marker,
            heartbeat,
        } => {
            run_daemon(
                client,
                &name,
                sample_seconds,
                frame_seconds,
                DaemonControl {
                    own_actuators,
                    guard_marker,
                    heartbeat,
                },
            )
            .await?
        }
        _ => unreachable!("handled before the runtime starts"),
    }
    Ok(())
}

/// The actuator-related half of `run`'s configuration.
struct DaemonControl {
    own_actuators: bool,
    guard_marker: PathBuf,
    heartbeat: PathBuf,
}

/// Touch the heartbeat file. This is the only thing keeping the failsafe asleep.
///
/// Written before anything else each tick, and deliberately not conditional on the
/// brain being reachable: an agent that is alive and merely offline is still driving
/// the garden correctly from its resident schedule, and letting the guard seize the
/// pins because the LAN is down would be a self-inflicted outage.
fn beat(heartbeat: &Heartbeat, note: &str) {
    if let Err(e) = heartbeat.touch(note) {
        tracing::warn!(%e, path = %heartbeat.path().display(), "cannot write the heartbeat");
    }
}

/// What the heartbeat file says, beyond the fact that it is recent.
///
/// The guard only cares about the mtime, but a person reading `/run/gardyn` at three
/// in the morning wants to know what the agent thinks it is driving — and if the pins
/// disagree with this, the problem is the wiring rather than the software.
fn heartbeat_note(actuators: Option<&actuators::OwnedActuators>) -> String {
    match actuators {
        Some(a) => format!(
            "{AGENT_VERSION} light={:.0}% pump={:.0}%",
            a.light().percent(),
            a.pump().percent()
        ),
        None => format!("{AGENT_VERSION} read-only"),
    }
}

/// Local seconds since midnight, for the resident schedule.
///
/// The schedule is in local hours because that is how a person thinks about when their
/// lights should come on. A Pi with the wrong timezone is therefore a real failure
/// mode, which is why the applied setpoint is logged with the hour it was computed for.
fn seconds_since_local_midnight(now: Timestamp) -> u32 {
    let zoned = now.to_zoned(jiff::tz::TimeZone::system());
    let hour = zoned.hour().clamp(0, 23) as u32;
    let minute = zoned.minute().clamp(0, 59) as u32;
    let second = zoned.second().clamp(0, 59) as u32;
    hour * 3600 + minute * 60 + second
}

async fn run_daemon(
    client: Client,
    name: &str,
    sample_seconds: u64,
    frame_seconds: u64,
    control: DaemonControl,
) -> Result<(), Box<dyn std::error::Error>> {
    let sample_interval = Duration::from_secs(sample_seconds.max(5));
    let component = client.register(name, sample_seconds as i64).await?;
    tracing::info!(
        %component,
        spool = %client.spool_dir().display(),
        backlog = client.spooled_count(),
        "registered with the brain"
    );

    // The resident schedule. Starts at the default and is replaced by whatever the
    // brain sends; it is never cleared, because "no opinion" must not mean "dark".
    let heartbeat = Heartbeat::new(&control.heartbeat);
    let mut schedule = Schedule::DEFAULT;
    let mut actuators = if control.own_actuators {
        tracing::warn!(
            "actuator control is ON — this agent is driving the lights and pump, so \
             the factory firmware must already be disabled"
        );
        match actuators::OwnedActuators::open(gardyn_hal::GuardMarker::new(
            &control.guard_marker,
        )) {
            Ok(driver) => Some(driver),
            Err(e) => {
                // Refusing to start would be worse: telemetry is still useful, and the
                // failsafe picks up the pins once the heartbeat stops.
                tracing::error!(%e, "cannot take the pins; continuing read-only");
                None
            }
        }
    } else {
        tracing::info!("read-only: the factory firmware still owns the lights and pump");
        None
    };

    let mut since_frame = Duration::ZERO;
    loop {
        let now = Timestamp::now();
        // Beaten before anything else, so a slow sensor read or a stalled upload can
        // never look like a dead agent to the failsafe.
        beat(&heartbeat, &heartbeat_note(actuators.as_ref()));

        if let Some(driver) = actuators.as_mut() {
            let seconds = seconds_since_local_midnight(now);
            let setpoint = schedule.setpoint(seconds);
            match driver.apply(setpoint) {
                Ok(actuators::Applied::Changed) => tracing::info!(
                    local_hour = seconds / 3600,
                    light = setpoint.light.percent(),
                    pump = setpoint.pump.percent(),
                    "setpoint applied"
                ),
                Ok(_) => {}
                Err(e) => tracing::error!(%e, "could not drive the pins"),
            }
            // Re-beat so the note carries what was just applied rather than the
            // previous tick's values.
            beat(&heartbeat, &heartbeat_note(actuators.as_ref()));
        }

        let snapshot = hardware::read_sensors(now);

        // Clear any backlog first, so history stays in order after an outage.
        match client.drain_spool().await {
            Ok(0) => {}
            Ok(n) => tracing::info!(replayed = n, "sent buffered samples"),
            Err(e) => tracing::warn!(%e, "spool replay failed; will retry"),
        }

        let status = match client.send_telemetry(&snapshot).await {
            Ok(accepted) => {
                if let Some(sent) = accepted.schedule {
                    // Validated before adoption. This is the only message the brain
                    // can send that changes what the hardware does, so a malformed one
                    // is refused rather than clamped into something plausible.
                    match sent.validate() {
                        Ok(()) if sent != schedule => {
                            tracing::info!(
                                light_hours = sent.light_hours,
                                daily_duty_hours = sent.daily_duty_hours(),
                                "adopted a new schedule from the brain"
                            );
                            schedule = sent;
                        }
                        Ok(()) => {}
                        Err(e) => tracing::error!(%e, "refused the schedule the brain sent"),
                    }
                }
                HeartbeatRequest::ok(AGENT_VERSION)
            }
            Err(e) => {
                tracing::warn!(%e, spooled = client.spooled_count(), "telemetry buffered");
                HeartbeatRequest::degraded(AGENT_VERSION, e.to_string())
            }
        };
        if let Err(e) = client.heartbeat(&component, &status).await {
            tracing::warn!(%e, "heartbeat failed");
        }

        if frame_seconds > 0 && since_frame >= Duration::from_secs(frame_seconds) {
            since_frame = Duration::ZERO;
            match camera::capture() {
                Ok(frame) => {
                    if let Err(e) = client
                        .upload_frame(
                            frame.bytes,
                            frame.captured_at,
                            frame.width,
                            frame.height,
                            None,
                            false,
                        )
                        .await
                    {
                        // Frames are not spooled: they are large, and a missing hourly
                        // photograph costs far less than a full SD card.
                        tracing::warn!(%e, "frame upload failed; dropping this one");
                    }
                }
                Err(e) => tracing::warn!(%e, "capture failed"),
            }
        }

        tokio::time::sleep(sample_interval).await;
        since_frame += sample_interval;
    }
}
