//! The operator's tool: calibration, event logging, and rule replay.
//!
//! Talks to the database directly rather than through the web API. It is an
//! administrative tool run on the brain's own VM, and direct access is what makes
//! replay possible at all — reconstructing a past `GardenState` needs the whole
//! history, not the handful of endpoints a browser needs.
//!
//! SQLite in WAL mode allows a second process, so this is safe to run against a
//! database the server is using. Writes are single statements or transactions.

mod calibrate;
mod replay;

use clap::{Args, Parser, Subcommand};
use garden_core::{GardenId, TankEvent, TankGeometry, Timestamp, time::add_days};
use garden_hal::Schedule;
use garden_store::Store;
use garden_vision::roi::RoiMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "garden-cli", version, about = "Garden operator tool")]
struct Cli {
    /// The brain's database. Must match the server's GARDEN_DB.
    #[arg(
        long,
        env = "GARDEN_DB",
        global = true,
        default_value = "sqlite://garden.db"
    )]
    database: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the gardens on this server and their ids.
    Gardens,

    /// Fit the tank's distance-to-volume mapping from measurements.
    #[command(subcommand)]
    Tank(TankCommand),

    /// Set up and check the camera's per-slot regions.
    #[command(subcommand)]
    Vision(VisionCommand),

    /// Record something you did to the tank.
    ///
    /// The rule engine re-derives "overdue for a refresh" from the last recorded
    /// action every time it runs, so an action you did not record did not happen and
    /// the task comes straight back.
    #[command(subcommand)]
    Log(LogCommand),

    /// The light and pump programme the Pi runs. **Phase 6.**
    #[command(subcommand)]
    Schedule(ScheduleCommand),

    /// Suggest what to plant in each empty slot, spreading the harvests out.
    Plan {
        #[arg(long)]
        garden: String,
        /// Candidates to show per slot.
        #[arg(long, default_value_t = 3)]
        top: usize,
    },

    /// Replay stored history against the current rule set.
    Replay(ReplayArgs),

    /// Write a consistent copy of the database.
    Backup {
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum TankCommand {
    /// Fit from `distance_mm:volume_l` pairs.
    ///
    /// Measure with the sensor's own reading, not a tape measure — you are calibrating
    /// what this sensor reports, including whatever offset it has.
    ///
    ///   garden-cli tank calibrate --capacity 15.5 330:0 240:5 150:10 60:15
    Calibrate {
        /// Usable capacity in litres.
        #[arg(long, default_value_t = 15.5)]
        capacity: f32,
        /// One or more `distance_mm:volume_l` pairs.
        #[arg(required = true)]
        samples: Vec<String>,
    },
    /// Show the tank as the rules currently see it.
    Show {
        #[arg(long)]
        garden: String,
    },
}

#[derive(Subcommand)]
enum VisionCommand {
    /// Write a starting ROI map for a garden, sized to a frame.
    Init {
        #[arg(long)]
        garden: String,
        #[arg(long)]
        width: u32,
        #[arg(long)]
        height: u32,
        /// Gutter between slot rectangles, as a fraction of each cell.
        #[arg(long, default_value_t = 0.12)]
        margin: f32,
        #[arg(long, default_value = "rois.json")]
        out: PathBuf,
    },
    /// Draw the rectangles onto a frame so they can be checked by eye.
    Preview {
        #[arg(long, default_value = "rois.json")]
        map: PathBuf,
        /// A frame from the garden, as a file.
        #[arg(long)]
        frame: PathBuf,
        #[arg(long, default_value = "rois-preview.png")]
        out: PathBuf,
    },
    /// Record how many centimetres a pixel covers, from something of known size.
    Scale {
        #[arg(long, default_value = "rois.json")]
        map: PathBuf,
        /// Real width of the reference object, in centimetres.
        #[arg(long)]
        cm: f32,
        /// How many pixels it spans in the frame.
        #[arg(long)]
        px: f32,
    },
    /// Upload the map to the brain, which switches vision on for that garden.
    Apply {
        #[arg(long)]
        garden: String,
        #[arg(long, default_value = "rois.json")]
        map: PathBuf,
    },
    /// Run the pipeline over a frame and print what it measured, storing nothing.
    Test {
        #[arg(long, default_value = "rois.json")]
        map: PathBuf,
        #[arg(long)]
        frame: PathBuf,
    },
    /// Turn vision off by forgetting where the slots are.
    Clear {
        #[arg(long)]
        garden: String,
    },
}

#[derive(Subcommand)]
enum ScheduleCommand {
    /// Show what the garden is being told to run.
    Show {
        #[arg(long)]
        garden: String,
    },
    /// Set the programme. Takes effect on the agent's next telemetry call.
    Set {
        #[arg(long)]
        garden: String,
        /// Local hour the lights start ramping up.
        #[arg(long, default_value_t = 6)]
        start_hour: u8,
        /// Hours of light per day, ramps included.
        #[arg(long, default_value_t = 16.0)]
        hours: f32,
        /// Duty at full daylight, 0.0 to 1.0.
        #[arg(long, default_value_t = 0.85)]
        duty: f32,
        /// Minutes spent ramping at dawn and at dusk.
        #[arg(long, default_value_t = 30.0)]
        ramp: f32,
        /// Minutes the pump runs at the start of each cycle.
        #[arg(long, default_value_t = 15.0)]
        pump_on: f32,
        /// Length of one pump cycle, in minutes.
        #[arg(long, default_value_t = 60.0)]
        pump_cycle: f32,
        /// Pump duty. Capped at 0.30 whatever is asked for.
        #[arg(long, default_value_t = 0.25)]
        pump_duty: f32,
    },
    /// Print the whole day, hour by hour, without changing anything.
    Preview {
        #[arg(long)]
        garden: String,
    },
    /// Stop sending a schedule. The agent keeps running whatever it last received.
    Clear {
        #[arg(long)]
        garden: String,
    },
}

#[derive(Subcommand)]
enum LogCommand {
    /// Water added, in litres.
    TopOff {
        #[arg(long)]
        garden: String,
        #[arg(long)]
        litres: f32,
        #[command(flatten)]
        when: When,
    },
    /// Plant food added, bringing the solution to full strength.
    Feed {
        #[arg(long)]
        garden: String,
        /// Fraction of full strength, for a sprout dose.
        #[arg(long, default_value_t = 1.0)]
        strength: f32,
        #[command(flatten)]
        when: When,
    },
    /// Water conditioner or HydroBoost.
    Condition {
        #[arg(long)]
        garden: String,
        #[command(flatten)]
        when: When,
    },
    /// Tank emptied and refilled.
    Refresh {
        #[arg(long)]
        garden: String,
        #[arg(long)]
        fill_to: Option<f32>,
        #[command(flatten)]
        when: When,
    },
    /// Full strip-down and scrub.
    Clean {
        #[arg(long)]
        garden: String,
        #[command(flatten)]
        when: When,
    },
    /// Show what has been recorded.
    Show {
        #[arg(long)]
        garden: String,
    },
    /// Remove a mis-logged entry by id.
    Undo {
        #[arg(long)]
        garden: String,
        id: String,
    },
}

#[derive(Args, Clone, Copy)]
struct When {
    /// How long ago it happened, in days. Defaults to now.
    #[arg(long, default_value_t = 0.0)]
    days_ago: f64,
}

impl When {
    fn resolve(self) -> Timestamp {
        add_days(Timestamp::now(), -self.days_ago.max(0.0))
    }
}

#[derive(Args)]
struct ReplayArgs {
    #[arg(long)]
    garden: String,
    /// How far back to start.
    #[arg(long, default_value_t = 90.0)]
    days: f64,
    /// Pretend this hardware were fitted. Repeatable.
    ///
    ///   --capability conductivity --capability canopy-metrics
    #[arg(long = "capability")]
    capabilities: Vec<String>,
    /// Print every task as it first appears, not just the summary.
    #[arg(long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

type Fallible = Result<(), Box<dyn std::error::Error>>;

async fn run() -> Fallible {
    let cli = Cli::parse();

    // Calibration arithmetic needs no database, and requiring one would mean you
    // could not work out your tank constants on a laptop.
    if let Command::Tank(TankCommand::Calibrate { capacity, samples }) = &cli.command {
        return tank_calibrate(*capacity, samples);
    }
    if let Command::Vision(command) = &cli.command
        && let Some(result) = vision_offline(command)
    {
        return result;
    }

    let store = Store::open(&cli.database).await?;

    match cli.command {
        Command::Gardens => gardens(&store).await,
        Command::Tank(TankCommand::Show { garden }) => tank_show(&store, &garden).await,
        Command::Tank(TankCommand::Calibrate { .. }) => unreachable!("handled above"),
        Command::Vision(command) => vision_online(&store, command).await,
        Command::Log(command) => log(&store, command).await,
        Command::Schedule(command) => schedule(&store, command).await,
        Command::Plan { garden, top } => plan(&store, &garden, top).await,
        Command::Replay(args) => replay_cmd(&store, args).await,
        Command::Backup { out } => {
            let path = out.to_string_lossy().to_string();
            store.backup_to(&path).await?;
            println!("wrote {path}");
            Ok(())
        }
    }
}

// --- Gardens -------------------------------------------------------------------------

async fn gardens(store: &Store) -> Fallible {
    let gardens = store.all_gardens().await?;
    if gardens.is_empty() {
        println!("no gardens yet — add one in the web UI");
        return Ok(());
    }
    for garden in gardens {
        let calibrated = store.roi_map(garden.id).await?.is_some();
        println!(
            "{}  {:<20} {:<10} vision:{}",
            garden.id,
            garden.name,
            garden.model.to_string(),
            if calibrated { "on" } else { "off" }
        );
    }
    Ok(())
}

fn parse_garden(raw: &str) -> Result<GardenId, String> {
    raw.parse()
        .map_err(|_| format!("'{raw}' is not a garden id — run `garden-cli gardens`"))
}

// --- Tank ----------------------------------------------------------------------------

fn tank_calibrate(capacity: f32, samples: &[String]) -> Fallible {
    let mut parsed = Vec::new();
    for raw in samples {
        let (distance, volume) = raw
            .split_once(':')
            .ok_or_else(|| format!("'{raw}' should look like 330:0 (distance_mm:volume_l)"))?;
        parsed.push(calibrate::TankSample {
            distance_mm: distance.trim().parse()?,
            volume_l: volume.trim().parse()?,
        });
    }

    let fitted = calibrate::fit_tank(&parsed, capacity)?;
    let residual = calibrate::worst_residual_l(&fitted, &parsed);

    println!("Fitted from {} measurements:\n", parsed.len());
    println!("    capacity_l:        {:.2}", fitted.capacity_l);
    println!("    full_distance_mm:  {:.1}", fitted.full_distance_mm);
    println!("    empty_distance_mm: {:.1}", fitted.empty_distance_mm);
    println!("\n    worst residual:    {residual:.2} L");

    if residual > capacity * 0.04 {
        println!(
            "\nThat residual is high. One measurement is probably off — an ultrasonic\n\
             sensor reading a moving surface is noisy, so let the water settle and\n\
             re-measure the outlier rather than accepting this."
        );
    }
    println!(
        "\nPut these in TankGeometry::STUDIO_2 in crates/garden-core/src/tank.rs.\n\
         They are the placeholder constants the water rules currently trust."
    );
    Ok(())
}

async fn tank_show(store: &Store, garden: &str) -> Fallible {
    let garden = parse_garden(garden)?;
    let now = Timestamp::now();
    let geometry = TankGeometry::STUDIO_2;
    let tank = store.tank_state_at(garden, &geometry, now).await?;

    let since = |label: &str, at: Option<Timestamp>| match at {
        Some(at) => println!(
            "    {label:<14} {:.1} days ago",
            garden_core::time::days_between(at, now)
        ),
        None => println!("    {label:<14} never recorded"),
    };

    println!("    volume         {:.1} L", tank.volume_l);
    println!("    strength       {:.0}%", tank.estimated_strength() * 100.0);
    println!(
        "    added since    {:.1} L (drives dosing when there is no EC probe)",
        tank.litres_added_since_food_dose
    );
    since("topped off", tank.last_top_off);
    since("fed", tank.last_food_dose);
    since("conditioned", tank.last_conditioner);
    since("refreshed", tank.last_refresh);
    since("deep cleaned", tank.last_deep_clean);
    Ok(())
}

// --- Vision ---------------------------------------------------------------------------

/// The subcommands that need no database. Returns `None` for the ones that do.
fn vision_offline(command: &VisionCommand) -> Option<Fallible> {
    match command {
        VisionCommand::Preview { map, frame, out } => Some(vision_preview(map, frame, out)),
        VisionCommand::Scale { map, cm, px } => Some(vision_scale(map, *cm, *px)),
        VisionCommand::Test { map, frame } => Some(vision_test(map, frame)),
        _ => None,
    }
}

fn read_map(path: &Path) -> Result<RoiMap, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

fn write_map(path: &Path, map: &RoiMap) -> Fallible {
    std::fs::write(path, serde_json::to_string_pretty(map)?)?;
    Ok(())
}

fn vision_preview(map_path: &Path, frame: &Path, out: &Path) -> Fallible {
    let map = read_map(map_path)?;
    let image = image::open(frame)?.to_rgb8();
    map.validate(image.width(), image.height())?;

    calibrate::write_png(&calibrate::overlay(&image, &map), out)?;
    println!("wrote {}", out.display());
    println!(
        "\nOpen it and check every rectangle sits over its own yPod. The tick marks in\n\
         the top-left corner count the slot number, so slot 3 has three pixels. Edit\n\
         {} and run this again until they line up.",
        map_path.display()
    );
    Ok(())
}

fn vision_scale(map_path: &Path, cm: f32, px: f32) -> Fallible {
    let mut map = read_map(map_path)?;
    calibrate::set_scale(&mut map, cm, px);
    write_map(map_path, &map)?;
    println!(
        "{cm} cm over {px} px — {:.5} cm² per pixel",
        map.slots.first().map(|s| s.cm2_per_px).unwrap_or(0.0)
    );
    if !map.is_calibrated() {
        println!("that produced the placeholder scale; check the numbers");
    }
    Ok(())
}

fn vision_test(map_path: &Path, frame: &Path) -> Fallible {
    let map = read_map(map_path)?;
    let calibrated = map.is_calibrated();
    let image = image::open(frame)?.to_rgb8();
    let report = garden_vision::Analyzer::new(map).analyse_image(&image, Timestamp::now())?;

    println!("slot   area      green   yellow  plants");
    for m in &report.slots {
        println!(
            "{:>4}   {:>7.1}   {:>5.0}%  {:>5.0}%  {}",
            m.slot.0,
            m.canopy_area_cm2,
            m.green_fraction * 100.0,
            m.yellowing_index * 100.0,
            m.plant_count
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
        );
    }
    for (slot, why) in &report.skipped {
        println!("{:>4}   not measured: {why}", slot.0);
    }
    if let Some(algae) = report.algae {
        println!("\nalgae coverage {:.0}%", algae.coverage * 100.0);
    }
    if !calibrated {
        println!(
            "\nAreas are in placeholder units — this map has no measured scale, so they\n\
             are comparable over time but not real cm². Run `vision scale` to fix that."
        );
    }
    Ok(())
}

async fn vision_online(store: &Store, command: VisionCommand) -> Fallible {
    match command {
        VisionCommand::Init {
            garden,
            width,
            height,
            margin,
            out,
        } => {
            let id = parse_garden(&garden)?;
            let found = store.find_garden(id).await?.ok_or("no such garden")?;
            let geometry = geometry_for(found.model);
            let map = calibrate::starting_map(&geometry, width, height, margin);
            write_map(&out, &map)?;
            println!(
                "wrote {} — {} slots on a {width}×{height} frame",
                out.display(),
                map.slots.len()
            );
            println!(
                "\nThis is an even grid and will not be right: a real tower is not\n\
                 axis-aligned in the frame. Next:\n\n\
                 \x20 1. garden-cli vision preview --frame <a frame from the garden>\n\
                 \x20 2. edit the rectangles in {}, repeat until they line up\n\
                 \x20 3. garden-cli vision scale --cm <known width> --px <its pixels>\n\
                 \x20 4. garden-cli vision apply --garden {}",
                out.display(),
                found.id
            );
            Ok(())
        }
        VisionCommand::Apply { garden, map } => {
            let id = parse_garden(&garden)?;
            let parsed = read_map(&map)?;
            parsed.validate(parsed.frame_width, parsed.frame_height)?;
            store
                .save_roi_map(id, &serde_json::to_string(&parsed)?, Timestamp::now())
                .await?;
            println!(
                "vision is on for {id}: {} slots, {}",
                parsed.slots.len(),
                if parsed.is_calibrated() {
                    "real cm²"
                } else {
                    "placeholder scale — areas are relative only"
                }
            );
            println!("The next frame the agent uploads will be measured.");
            Ok(())
        }
        VisionCommand::Clear { garden } => {
            let id = parse_garden(&garden)?;
            store.clear_roi_map(id).await?;
            println!("vision off for {id}; measurements already taken are kept");
            Ok(())
        }
        _ => unreachable!("offline subcommands are handled before the database opens"),
    }
}

fn geometry_for(model: garden_core::DeviceModel) -> garden_core::Geometry {
    use garden_core::{DeviceModel, Geometry};
    match model {
        DeviceModel::Home4 | DeviceModel::Home3 => Geometry {
            columns: 3,
            rows_per_column: 10,
        },
        _ => Geometry::STUDIO_2,
    }
}

// --- Logging ---------------------------------------------------------------------------

async fn log(store: &Store, command: LogCommand) -> Fallible {
    let geometry = TankGeometry::STUDIO_2;

    let (garden, event, at) = match command {
        LogCommand::TopOff {
            garden,
            litres,
            when,
        } => (garden, TankEvent::TopOff { litres }, when.resolve()),
        LogCommand::Feed {
            garden,
            strength,
            when,
        } => (
            garden,
            TankEvent::FedToStrength { strength },
            when.resolve(),
        ),
        LogCommand::Condition { garden, when } => (garden, TankEvent::Conditioner, when.resolve()),
        LogCommand::Refresh {
            garden,
            fill_to,
            when,
        } => (
            garden,
            TankEvent::Refresh {
                fill_to_l: fill_to.unwrap_or(geometry.capacity_l),
            },
            when.resolve(),
        ),
        LogCommand::Clean { garden, when } => (garden, TankEvent::DeepClean, when.resolve()),

        LogCommand::Show { garden } => {
            let id = parse_garden(&garden)?;
            let events = store.tank_events(id, Timestamp::now()).await?;
            if events.is_empty() {
                println!("nothing recorded yet");
            }
            for record in events {
                println!(
                    "{}  {}  {}",
                    record.id,
                    record.occurred_at,
                    record.event.label()
                );
            }
            return Ok(());
        }
        LogCommand::Undo { garden, id } => {
            let garden = parse_garden(&garden)?;
            let uuid: uuid::Uuid = id.parse()?;
            if store.delete_tank_event(garden, uuid).await? {
                println!("removed; the tank state recomputes without it");
            } else {
                println!("no such entry in that garden");
            }
            return Ok(());
        }
    };

    let id = parse_garden(&garden)?;
    // `actor: None` — this was done at the command line, not by a signed-in person,
    // and inventing an account for it would put a name against work nobody claimed.
    store.record_tank_event(id, event, None, at).await?;

    let tank = store.tank_state_at(id, &geometry, Timestamp::now()).await?;
    println!(
        "recorded: {} at {at}\n    strength now {:.0}%, {:.1} L added since the last feed",
        event.label(),
        tank.estimated_strength() * 100.0,
        tank.litres_added_since_food_dose
    );
    Ok(())
}

// --- Schedule ---------------------------------------------------------------------------

async fn schedule(store: &Store, command: ScheduleCommand) -> Fallible {
    match command {
        ScheduleCommand::Show { garden } => {
            let id = parse_garden(&garden)?;
            match store.schedule(id).await? {
                Some(s) => print_schedule(&s),
                None => println!(
                    "no schedule set — the agent runs whatever it last received, or its\n                     own default if it has never been told. Clearing does not mean dark."
                ),
            }
            Ok(())
        }
        ScheduleCommand::Set {
            garden,
            start_hour,
            hours,
            duty,
            ramp,
            pump_on,
            pump_cycle,
            pump_duty,
        } => {
            let id = parse_garden(&garden)?;
            let wanted = Schedule {
                light_start_hour: start_hour,
                light_hours: hours,
                light_duty: duty,
                ramp_minutes: ramp,
                pump_on_minutes: pump_on,
                pump_cycle_minutes: pump_cycle,
                pump_duty,
            };
            wanted.validate()?;

            if let Some(current) = store.schedule(id).await? {
                let before = current.daily_duty_hours();
                let after = wanted.daily_duty_hours();
                println!(
                    "daily light {before:.1} → {after:.1} duty-hours ({:+.0}%)\n",
                    if before > 0.0 {
                        (after - before) / before * 100.0
                    } else {
                        0.0
                    }
                );
            }

            store.set_schedule(id, &wanted, Timestamp::now()).await?;
            print_schedule(&wanted);
            println!("\nThe agent picks this up on its next telemetry call. It only acts on it");
            println!("if it was started with --own-actuators.");
            Ok(())
        }
        ScheduleCommand::Preview { garden } => {
            let id = parse_garden(&garden)?;
            let s = store.schedule(id).await?.unwrap_or(Schedule::DEFAULT);
            println!("hour   light   pump");
            for hour in 0..24 {
                let point = s.setpoint(hour * 3600);
                println!(
                    "{hour:>4}   {:>5.0}%  {:>5.0}%",
                    point.light.percent(),
                    point.pump.percent()
                );
            }
            Ok(())
        }
        ScheduleCommand::Clear { garden } => {
            let id = parse_garden(&garden)?;
            store.clear_schedule(id).await?;
            println!("cleared; the agent keeps running whatever it last received");
            Ok(())
        }
    }
}

fn print_schedule(s: &Schedule) {
    println!("    lights         {:02}:00 for {:.1} h at {:.0}%", s.light_start_hour, s.light_hours, s.light_duty * 100.0);
    println!("    ramp           {:.0} min at each end", s.ramp_minutes);
    println!(
        "    pump           {:.0} min in every {:.0} at {:.0}%",
        s.pump_on_minutes,
        s.pump_cycle_minutes,
        s.pump_duty * 100.0
    );
    println!("    daily light    {:.1} duty-hours", s.daily_duty_hours());
}

// --- Plan -------------------------------------------------------------------------------

async fn plan(store: &Store, garden: &str, top: usize) -> Fallible {
    let id = parse_garden(garden)?;
    let found = store.find_garden(id).await?.ok_or("no such garden")?;
    let now = Timestamp::now();

    let mut state = garden_core::GardenState::for_garden(id, now);
    state.geometry = geometry_for(found.model);
    state.plantings = store.active_plantings(id).await?;

    println!("{} — what to plant where\n", found.name);

    // The same list the planner reasons over, so the advice below can be checked
    // rather than taken on trust. Computing it separately here would let the display
    // and the planner disagree, which reads as the advice being wrong.
    let coming = garden_rules::succession::coming_harvests(&state);

    if coming.is_empty() {
        println!("Nothing is due to be harvested, so anything you plant sets the rhythm.\n");
    } else {
        println!("already coming:");
        for (name, days) in &coming {
            println!("    day {days:>4.0}   {name}");
        }
        println!();
    }

    let empty = state.empty_slots();
    if empty.is_empty() {
        println!("Every slot is full. Nothing to plan until one frees up.");
        return Ok(());
    }

    // Planned as a tower rather than slot by slot: each choice is made knowing the
    // ones before it, or every empty slot gets the same answer.
    for (slot, ranked) in garden_rules::succession::plan_tower(&state, top) {
        let zone = state.geometry.light_zone(slot);
        println!("{}  ({})", slot, zone.label());
        if ranked.is_empty() {
            println!("    nothing in the catalogue suits this slot");
        }
        for suggestion in ranked {
            println!(
                "    {:<24} harvest ~day {:>3.0}   {}",
                suggestion.name, suggestion.harvest_in_days, suggestion.reason
            );
        }
        println!();
    }

    println!(
        "Ranked by how far each harvest lands from the ones already coming. Sixteen\n\
         slots planted the same afternoon give sixteen harvests in one week and then\n\
         nothing for a month; this is where that gets avoided."
    );
    Ok(())
}

// --- Replay -----------------------------------------------------------------------------

async fn replay_cmd(store: &Store, args: ReplayArgs) -> Fallible {
    let garden = parse_garden(&args.garden)?;
    let found = store.find_garden(garden).await?.ok_or("no such garden")?;

    let mut extra = Vec::new();
    for name in &args.capabilities {
        let capability =
            replay::parse_capability(name).ok_or_else(|| format!("'{name}' is not a capability"))?;
        extra.push(capability);
    }

    let now = Timestamp::now();
    let from = add_days(now, -args.days.max(0.0));
    let summary = replay::run(
        store,
        garden,
        geometry_for(found.model),
        &garden_rules::default_engine(),
        from,
        now,
        &extra,
    )
    .await?;

    println!("{} — {:.0} days to {now}", found.name, args.days);
    if !extra.is_empty() {
        let names: Vec<&str> = extra.iter().map(|c| c.label()).collect();
        println!("assuming fitted: {}", names.join(", "));
    }
    println!();

    if args.verbose {
        for day in &summary.days {
            for (kind, rationale) in &day.new_tasks {
                println!("{}  {kind:<22} {rationale}", day.at);
            }
        }
        println!();
    }

    println!("first raised");
    for (kind, at) in &summary.first_seen {
        println!(
            "    {kind:<22} day {:.0}",
            garden_core::time::days_between(from, *at)
        );
    }
    println!("\ntimes raised");
    for (kind, count) in &summary.totals {
        println!("    {kind:<22} {count}");
    }
    println!(
        "\n    {} tasks over {} days",
        summary.total_tasks(),
        summary.days.len()
    );

    if summary.blind_days > 0 {
        println!(
            "\n{} of {} days had no sensor reading. Rules needing telemetry could not\n\
             run on those days, so this replay mostly measures the gap.",
            summary.blind_days,
            summary.days.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_tree_is_well_formed() {
        // clap validates argument definitions here rather than at first use, so a
        // duplicated flag or a bad default is caught by `cargo test`, not by a user.
        Cli::command().debug_assert();
    }

    #[test]
    fn a_relative_time_resolves_into_the_past() {
        let now = Timestamp::now();
        let two_days = When { days_ago: 2.0 }.resolve();
        let elapsed = garden_core::time::days_between(two_days, now);
        assert!((elapsed - 2.0).abs() < 0.01, "{elapsed}");
    }

    #[test]
    fn a_negative_days_ago_does_not_record_the_future() {
        // Logging an action that has not happened yet would silence a task before the
        // work was done.
        let resolved = When { days_ago: -5.0 }.resolve();
        assert!(resolved <= Timestamp::now());
    }

    #[test]
    fn a_bad_garden_id_says_how_to_find_a_good_one() {
        let error = parse_garden("kitchen").unwrap_err();
        assert!(error.contains("garden-cli gardens"), "{error}");
    }
}
