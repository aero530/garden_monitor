//! The Garden edge agent.
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
mod ultrasonic;
mod vendor;

use brain::{AGENT_VERSION, Client};
use clap::{Parser, Subcommand};
use garden_core::{GardenId, Timestamp};
use garden_hal::{Heartbeat, Schedule};
use garden_proto::HeartbeatRequest;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "garden-edge", version, about = "Gardyn device agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Brain base URL.
    #[arg(
        long,
        env = "GARDEN_BRAIN_URL",
        global = true,
        default_value = "http://localhost:8080"
    )]
    brain_url: String,

    /// Shared agent token, matching GARDEN_AGENT_TOKEN on the brain.
    #[arg(long, env = "GARDEN_AGENT_TOKEN", global = true, default_value = "")]
    token: String,

    /// Which garden this device is.
    #[arg(long, env = "GARDEN_GARDEN_ID", global = true)]
    garden: Option<String>,

    /// Where unsent samples are buffered when the brain is unreachable.
    #[arg(
        long,
        env = "GARDEN_SPOOL_DIR",
        global = true,
        default_value = "/var/lib/garden/spool"
    )]
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
        /// and `garden-guard` has been proven.
        #[arg(long, env = "GARDEN_OWN_ACTUATORS", default_value_t = false)]
        own_actuators: bool,

        /// Where `garden-guard` says it has seized the pins.
        #[arg(
            long,
            env = "GARDEN_GUARD_MARKER",
            default_value = "/run/garden/guard.engaged"
        )]
        guard_marker: PathBuf,

        /// Milliseconds to wait after pinning the lights before the shutter.
        ///
        /// The LEDs settle almost instantly; the camera's auto-exposure does not, and
        /// a frame taken too early is darker than the one before it for no reason the
        /// plant is responsible for.
        #[arg(long, env = "GARDEN_PHOTO_SETTLE_MS", default_value_t = 600)]
        photo_settle_ms: u64,

        /// File touched every tick, which is what tells the guard we are alive.
        #[arg(
            long,
            env = "GARDEN_HEARTBEAT",
            default_value = "/run/garden/edge.heartbeat"
        )]
        heartbeat: PathBuf,

        /// Seconds between sensor samples.
        #[arg(long, env = "GARDEN_SAMPLE_SECONDS", default_value_t = 60)]
        sample_seconds: u64,
        /// Seconds between photographs. Zero disables the camera.
        #[arg(long, env = "GARDEN_FRAME_SECONDS", default_value_t = 3600)]
        frame_seconds: u64,
        /// Name this device registers under on the fleet page.
        #[arg(long, env = "GARDEN_AGENT_NAME", default_value = "garden-edge")]
        name: String,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "garden_edge=info".into()),
        )
        .init();

    let cli = Cli::parse();

    // `probe`, `read` and `watch-pwm` deliberately need no async runtime, no token and
    // no garden id. They are the commands you run on a device you have just opened,
    // possibly before the brain exists at all.
    match &cli.command {
        Command::Probe { out } => return probe(out),
        Command::Read => return read_once(),
        // `capture --out` writes a file and nothing else, so it has no business
        // demanding a token and a garden id. Taking one frame to look at is the first
        // thing anyone does with a camera on an unfamiliar device — on this one, to
        // find out which way up the `hw_gm20` profile's `rotatePhotos` leaves it.
        Command::Capture { out: Some(path) } if cli.token.is_empty() => {
            return capture_to_file(path);
        }
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

/// A duration as something to read rather than a pile of seconds.
fn humanise(age: Duration) -> String {
    let seconds = age.as_secs();
    match seconds {
        0..=90 => format!("{seconds}s"),
        91..=5400 => format!("{}m", seconds / 60),
        _ => format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

/// Take one frame and write it, with no brain and no network.
fn capture_to_file(path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let frame = camera::capture()?;
    std::fs::write(path, &frame.bytes)?;
    println!(
        "wrote {} — {}x{}, {} KiB",
        path.display(),
        frame.width,
        frame.height,
        frame.bytes.len() / 1024
    );
    println!("not uploaded: GARDEN_AGENT_TOKEN is unset, so this was a local capture only");
    Ok(())
}

fn probe(out: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let report = hardware::probe(AGENT_VERSION, Timestamp::now());
    let json = serde_json::to_string_pretty(&report)?;
    std::fs::write(out, &json)?;

    println!("Garden edge recon — agent {AGENT_VERSION}");
    println!();
    println!(
        "  board    {}",
        report.board_model.as_deref().unwrap_or("unknown")
    );
    println!(
        "  arch     {}",
        report.cpu_architecture.as_deref().unwrap_or("unknown")
    );
    println!("  os       {}", report.os.as_deref().unwrap_or("unknown"));
    println!(
        "  kernel   {}",
        report.kernel.as_deref().unwrap_or("unknown")
    );
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

    // The Phase 1 gate. Worth its own line rather than being left to the warnings,
    // because a parity capture that cannot see the pins fails silently for a fortnight.
    println!(
        "  PWM readable: {}",
        match (&report.pigpiod_interface, report.pwm_channels.is_empty()) {
            (Some(interface), _) => format!("yes, via pigpio's {interface}"),
            (None, false) => "yes, via /sys/class/pwm".to_string(),
            (None, true) => "NO — parity capture would record nothing".to_string(),
        }
    );

    if !report.gpio_modes.is_empty() {
        println!(
            "  GPIO: {} mapped, {} free{}",
            report.gpio_modes.len(),
            report.free_gpios().len(),
            match report.suggested_one_wire_gpio() {
                // Naming one is what makes this actionable — it is the pin the
                // DS18B20 overlay will point at.
                Some(pin) => format!(" — put the DS18B20 on GPIO{pin}"),
                None => String::new(),
            }
        );
        for (gpio, _, _) in garden_proto::recon::expected::PIN_ROLES {
            if let Some(pin) = report.gpio_modes.iter().find(|p| p.gpio == *gpio) {
                println!(
                    "    GPIO{:<2} {:<7} {}",
                    pin.gpio,
                    pin.mode,
                    pin.role.as_deref().unwrap_or("")
                );
            }
        }
    }

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
    println!(
        "capabilities this reading demonstrates: {}",
        capabilities.join(", ")
    );

    // The factory's own tank reading, printed raw whether or not it made it into the
    // snapshot. Until the direction is confirmed against a jug of water this number is
    // the evidence, and the interpreted one is a claim about it.
    match vendor::water_level() {
        Ok(level) => {
            println!();
            println!(
                "  factory tank reading: {:.1} cm, written {} ago{}",
                level.raw_cm,
                humanise(level.age),
                if level.writer_running {
                    " — gy_wl running, so an old timestamp just means a steady tank"
                } else {
                    " — gy_wl is NOT running; this will not update again"
                }
            );
            match level.distance_mm() {
                Some(mm) => println!(
                    "    read as {mm:.0} mm from the sensor down to the water — \
                     assumed, not proven.\n    \
                     Confirm it: note this number, add a litre, read again. It should FALL."
                ),
                None => println!(
                    "    not converted: the value is configured as a depth, and turning \
                     that into a\n    distance needs a calibrated tank"
                ),
            }
        }
        // Absent is the normal case off-device and says nothing interesting.
        Err(vendor::VendorError::Absent) => {}
        Err(e) => {
            println!();
            println!("  factory tank reading unavailable: {e}");
        }
    }

    // Called out on its own because it is the one sensor whose absence silently
    // disables a whole rule, and "null" in the JSON does not say why.
    if snapshot.water_level_mm.is_none() {
        println!();
        println!(
            "No water level. Without it the water rule cannot run, so nothing will
             ever tell you to top the tank up. Check, in order:
               - the warning above: `os error 22` is the kernel being older than the
                 GPIO v2 uAPI rppal needs, not a permissions problem
               - this binary is running on the device, not your workstation
               - the agent's user is in the `gpio` group (`id -nG`)
               - the sensor is on GPIO{trig} (trigger) and GPIO{echo} (echo)",
            trig = garden_proto::recon::expected::GPIO_ULTRASONIC_TRIG,
            echo = garden_proto::recon::expected::GPIO_ULTRASONIC_ECHO,
        );
    }
    Ok(())
}

async fn async_main(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.token.is_empty() {
        return Err("GARDEN_AGENT_TOKEN is not set; the brain will reject every request".into());
    }
    let garden: GardenId = cli
        .garden
        .as_deref()
        .ok_or("GARDEN_GARDEN_ID is not set — create the garden in the web UI first")?
        .parse()
        .map_err(|_| "GARDEN_GARDEN_ID is not a valid id")?;

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
            photo_settle_ms,
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
                    photo_settle: Duration::from_millis(photo_settle_ms),
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
    photo_settle: Duration,
}

/// One photograph, and what is known about the light it was taken under.
struct Shot {
    bytes: Vec<u8>,
    captured_at: Timestamp,
    width: u32,
    height: u32,
    /// `None` when we do not own the lights and therefore cannot know.
    light_duty_milli: Option<i64>,
    /// Whether this frame can be compared with others by colour.
    comparable: bool,
}

/// Whether a capture at this moment should pin the lights to the reference level.
///
/// Two conditions, and the second is the interesting one. Owning the actuators is
/// necessary but not sufficient: driving the bar to 80% at two in the morning to take
/// a photograph would wake the garden for the sake of a measurement, which is a worse
/// trade than an ambient frame. So a pinned capture only happens during the hours the
/// schedule already has the lights on.
fn should_pin(owns_actuators: bool, schedule: &Schedule, seconds_since_midnight: u32) -> bool {
    owns_actuators && !schedule.setpoint(seconds_since_midnight).light.is_off()
}

/// Take a photograph, pinning the lights when that is both possible and appropriate.
///
/// Falls back to an ambient capture whenever pinning is refused — the failsafe holding
/// the pins, say. A frame taken while the light level was not under our control must
/// not be labelled comparable, because the whole value of that flag is that a colour
/// difference between two comparable frames is the plant changing rather than the
/// lighting.
fn take_shot(
    actuators: Option<&mut actuators::OwnedActuators>,
    schedule: &Schedule,
    seconds_since_midnight: u32,
    settle: Duration,
) -> Result<Shot, camera::CameraError> {
    if let Some(driver) = actuators
        && should_pin(true, schedule, seconds_since_midnight)
    {
        match garden_hal::photo_mode(
            driver,
            &mut camera::HalCamera,
            garden_hal::PHOTO_REFERENCE,
            || std::thread::sleep(settle),
        ) {
            Ok(frame) => {
                return Ok(Shot {
                    bytes: frame.data,
                    captured_at: frame.captured_at,
                    width: frame.width,
                    height: frame.height,
                    light_duty_milli: Some(i64::from(frame.light_duty_milli)),
                    comparable: true,
                });
            }
            Err(error) => tracing::warn!(
                %error,
                "could not pin the lights; taking an ambient frame instead"
            ),
        }
    }

    let frame = camera::capture()?;
    Ok(Shot {
        bytes: frame.bytes,
        captured_at: frame.captured_at,
        width: frame.width,
        height: frame.height,
        light_duty_milli: None,
        comparable: false,
    })
}

/// Touch the heartbeat file. This is the only thing keeping the failsafe asleep.
///
/// Written before anything else each tick, and deliberately not conditional on the
/// brain being reachable: an agent that is alive and merely offline is still driving
/// the garden correctly from its resident schedule, and letting the guard seize the
/// pins because the LAN is down would be a self-inflicted outage.
fn beat(heartbeat: &Heartbeat, note: &str) {
    if let Err(e) = heartbeat.touch(note) {
        // Once, not every tick. The default path is under `/run`, which wants root, so
        // an unprivileged agent would otherwise repeat this every sample for months and
        // bury everything else in the log.
        //
        // Harmless in Phase 1 — nothing reads the heartbeat until `garden-guard` runs —
        // but it inverts at Phase 6. A guard that cannot see a heartbeat concludes the
        // agent is dead and seizes the pins, so an unwritable path there means the
        // failsafe fighting a perfectly healthy agent for the PWM lines.
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                %e,
                path = %heartbeat.path().display(),
                "cannot write the heartbeat, and will not say so again. Harmless until \
                 garden-guard runs; before Phase 6, set GARDEN_HEARTBEAT to a writable \
                 path the guard can also read."
            );
        });
    }
}

/// What the heartbeat file says, beyond the fact that it is recent.
///
/// The guard only cares about the mtime, but a person reading `/run/garden` at three
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

    // Registration is retried, not required.
    //
    // This was `client.register(...).await?` until 2026-08-31, when the brain happened
    // to be stopped and the agent exited at start-up with a connection timeout rather
    // than sampling anything. That contradicts §4's load-bearing rule: the Pi holds its
    // own schedule and keeps the garden alive whether or not the brain is there. And on
    // a device with no root, the daemon is started from an `@reboot` crontab — so
    // "died because a laptop was asleep" means nothing runs until the next reboot.
    //
    // Everything downstream already copes: telemetry spools, heartbeats warn, frames
    // are dropped. Registration was the one place that did not.
    let mut component = match client.register(name, sample_seconds as i64).await {
        Ok(component) => {
            tracing::info!(
                %component,
                spool = %client.spool_dir().display(),
                backlog = client.spooled_count(),
                "registered with the brain"
            );
            Some(component)
        }
        Err(e) => {
            tracing::warn!(
                %e,
                spool = %client.spool_dir().display(),
                "could not register; sampling anyway and retrying each tick. Telemetry \
                 will spool until the brain answers."
            );
            None
        }
    };

    // The resident schedule. Starts at the default and is replaced by whatever the
    // brain sends; it is never cleared, because "no opinion" must not mean "dark".
    let heartbeat = Heartbeat::new(&control.heartbeat);
    let mut schedule = Schedule::DEFAULT;
    let mut actuators = if control.own_actuators {
        tracing::warn!(
            "actuator control is ON — this agent is driving the lights and pump, so \
             the factory firmware must already be disabled"
        );
        match actuators::OwnedActuators::open(garden_hal::GuardMarker::new(&control.guard_marker)) {
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

    // Held open across ticks: the ultrasonic registers a kernel interrupt, and
    // re-registering it every minute would risk missing the edge being waited for.
    let mut sensors = hardware::Bank::open();

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

        let snapshot = sensors.read(now);

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
        // Catch up on a registration that could not happen at start-up. Cheap: it only
        // runs while unregistered, and the brain coming back is the common case.
        if component.is_none()
            && let Ok(registered) = client.register(name, sample_seconds as i64).await
        {
            tracing::info!(component = %registered, "registered with the brain");
            component = Some(registered);
        }
        if let Some(component) = component.as_ref()
            && let Err(e) = client.heartbeat(component, &status).await
        {
            tracing::warn!(%e, "heartbeat failed");
        }

        if frame_seconds > 0 && since_frame >= Duration::from_secs(frame_seconds) {
            since_frame = Duration::ZERO;
            let seconds = seconds_since_local_midnight(now);
            match take_shot(actuators.as_mut(), &schedule, seconds, control.photo_settle) {
                Ok(shot) => {
                    if shot.comparable {
                        tracing::debug!(light = shot.light_duty_milli, "pinned capture");
                    }
                    if let Err(e) = client
                        .upload_frame(
                            shot.bytes,
                            shot.captured_at,
                            shot.width,
                            shot.height,
                            shot.light_duty_milli,
                            shot.comparable,
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

#[cfg(test)]
mod photo_tests {
    use super::*;

    const HOUR: u32 = 3600;

    #[test]
    fn a_read_only_agent_never_pins() {
        // Phases 0-5 run alongside the factory firmware, which owns the lights. Pinning
        // them would be two processes fighting over one PWM line.
        let day = Schedule::DEFAULT;
        for hour in 0..24 {
            assert!(!should_pin(false, &day, hour * HOUR), "hour {hour}");
        }
    }

    #[test]
    fn a_daytime_capture_pins_and_a_night_one_does_not() {
        // Driving the bar to 80% at two in the morning to take a photograph would wake
        // the garden for the sake of a measurement. An ambient frame is the better
        // trade, and it is honestly labelled.
        let s = Schedule::DEFAULT; // 06:00, 16 hours, so dark from 22:00.
        assert!(should_pin(true, &s, 12 * HOUR), "midday");
        assert!(should_pin(true, &s, 20 * HOUR), "evening, still lit");
        assert!(!should_pin(true, &s, 2 * HOUR), "the small hours");
        assert!(!should_pin(true, &s, 23 * HOUR), "after lights out");
    }

    #[test]
    fn the_dawn_ramp_counts_as_lit() {
        // Part-way up the ramp the lights are on, just not at full. Pinning takes them
        // to the reference and back, so the ramp is not a reason to skip.
        let s = Schedule::DEFAULT;
        assert!(
            should_pin(true, &s, 6 * HOUR + 900),
            "fifteen minutes into dawn"
        );
        assert!(
            !should_pin(true, &s, 6 * HOUR - 60),
            "a minute before it starts"
        );
    }

    #[test]
    fn a_schedule_with_no_light_at_all_never_pins() {
        let dark = Schedule {
            light_hours: 0.0,
            ..Schedule::DEFAULT
        };
        for hour in 0..24 {
            assert!(!should_pin(true, &dark, hour * HOUR), "hour {hour}");
        }
    }

    #[test]
    fn an_agent_that_owns_no_actuators_takes_an_ambient_frame() {
        // No pins, so no capture tool is invoked on a desktop either — this asserts the
        // decision, which is the part that has to be right.
        let s = Schedule::DEFAULT;
        assert!(!should_pin(false, &s, 12 * HOUR));
    }
}
