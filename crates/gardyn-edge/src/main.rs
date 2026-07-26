//! The Gardyn edge agent.
//!
//! Runs on the device's Raspberry Pi. In Phase 1 it is strictly read-only: it reads
//! sensors, takes photographs, and reports to the brain, while the factory firmware
//! keeps running the lights and pump. Nothing here writes to an actuator, because two
//! processes contending for the same PWM pin is an excellent way to lose a tray of
//! seedlings.
//!
//! See HARDWARE.md for the full runbook.

mod brain;
mod camera;
mod hardware;
mod pwm_watch;

use brain::{AGENT_VERSION, Client};
use clap::{Parser, Subcommand};
use gardyn_core::{GardenId, Timestamp};
use gardyn_proto::HeartbeatRequest;
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
        } => run_daemon(client, &name, sample_seconds, frame_seconds).await?,
        _ => unreachable!("handled before the runtime starts"),
    }
    Ok(())
}

async fn run_daemon(
    client: Client,
    name: &str,
    sample_seconds: u64,
    frame_seconds: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let sample_interval = Duration::from_secs(sample_seconds.max(5));
    let component = client.register(name, sample_seconds as i64).await?;
    tracing::info!(
        %component,
        spool = %client.spool_dir().display(),
        backlog = client.spooled_count(),
        "registered with the brain"
    );

    let mut since_frame = Duration::ZERO;
    loop {
        let now = Timestamp::now();
        let snapshot = hardware::read_sensors(now);

        // Clear any backlog first, so history stays in order after an outage.
        match client.drain_spool().await {
            Ok(0) => {}
            Ok(n) => tracing::info!(replayed = n, "sent buffered samples"),
            Err(e) => tracing::warn!(%e, "spool replay failed; will retry"),
        }

        let status = match client.send_telemetry(&snapshot).await {
            Ok(_) => HeartbeatRequest::ok(AGENT_VERSION),
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
