//! End-to-end tests that run the compiled binary.
//!
//! The unit tests cover the arithmetic. These cover the part that only breaks in
//! practice: argument parsing, the database actually opening, and whether the output
//! tells you what to do next. Cargo hands us the built binary through
//! `CARGO_BIN_EXE_garden-cli`.

use garden_auth::EmailAddress;
use garden_core::{DeviceModel, GardenId, SlotId, Timestamp, VarietyId, time::add_days};
use garden_store::Store;
use std::path::PathBuf;
use std::process::Command;

fn t0() -> Timestamp {
    Timestamp::from_second(1_700_000_000).unwrap()
}

struct Fixture {
    dir: PathBuf,
    url: String,
    garden: GardenId,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn fixture(name: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("garden-cli-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    // sqlx wants forward slashes even on Windows.
    let url = format!("sqlite://{}/g.db", dir.display().to_string().replace('\\', "/"));

    let store = Store::open_with(&url, dir.join("frames")).await.unwrap();
    let user = store
        .create_user(
            EmailAddress::parse("phil@example.com").unwrap(),
            "Phil",
            "a long enough password",
            t0(),
        )
        .await
        .unwrap();
    let garden = store
        .create_garden("Kitchen", DeviceModel::Studio2, "UTC", user.id, t0())
        .await
        .unwrap();
    store
        .plant(
            garden.id,
            SlotId(0),
            &VarietyId::new("kale-lacinato"),
            add_days(Timestamp::now(), -40.0),
            16,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    store
        .record_planting_event(
            garden.id,
            garden_core::PlantingId(1),
            garden_store::plantings::PlantingEvent::Germinated,
            add_days(Timestamp::now(), -34.0),
        )
        .await
        .unwrap();

    Fixture {
        dir,
        url,
        garden: garden.id,
    }
}

fn run(fixture: &Fixture, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_garden-cli"))
        .args(args)
        .env("GARDEN_DB", &fixture.url)
        .output()
        .expect("run garden-cli");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "`garden-cli {}` failed\nstdout: {stdout}\nstderr: {stderr}",
        args.join(" ")
    );
    stdout
}

fn run_failing(fixture: &Fixture, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_garden-cli"))
        .args(args)
        .env("GARDEN_DB", &fixture.url)
        .output()
        .expect("run garden-cli");
    assert!(!output.status.success(), "expected a failure exit code");
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[tokio::test]
async fn gardens_lists_the_id_you_need_for_every_other_command() {
    let f = fixture("gardens").await;
    let out = run(&f, &["gardens"]);
    assert!(out.contains(&f.garden.to_string()));
    assert!(out.contains("Kitchen"));
    assert!(out.contains("vision:off"), "no calibration yet: {out}");
}

#[tokio::test]
async fn a_logged_action_moves_the_tank_and_shows_up_in_the_log() {
    let f = fixture("log").await;
    let garden = f.garden.to_string();

    assert!(run(&f, &["tank", "show", "--garden", &garden]).contains("never recorded"));

    run(&f, &["log", "feed", "--garden", &garden]);
    let shown = run(&f, &["tank", "show", "--garden", &garden]);
    assert!(shown.contains("fed"), "{shown}");
    assert!(!shown.contains("fed            never"), "{shown}");

    let log = run(&f, &["log", "show", "--garden", &garden]);
    assert!(log.contains("fed"), "{log}");
}

#[tokio::test]
async fn a_mis_logged_action_can_be_undone() {
    let f = fixture("undo").await;
    let garden = f.garden.to_string();

    run(&f, &["log", "clean", "--garden", &garden]);
    let log = run(&f, &["log", "show", "--garden", &garden]);
    let id = log.split_whitespace().next().expect("an event id");

    let undone = run(&f, &["log", "undo", "--garden", &garden, id]);
    assert!(undone.contains("recomputes"), "{undone}");
    assert!(
        run(&f, &["tank", "show", "--garden", &garden]).contains("deep cleaned   never"),
        "the clean should be gone"
    );
}

#[tokio::test]
async fn logging_backdated_work_is_honoured() {
    // You feed the tank on Saturday and record it on Monday. The rules care about
    // Saturday.
    let f = fixture("backdate").await;
    let garden = f.garden.to_string();

    run(&f, &["log", "feed", "--garden", &garden, "--days-ago", "9"]);
    let shown = run(&f, &["tank", "show", "--garden", &garden]);
    assert!(shown.contains("9.0 days ago"), "{shown}");
}

#[tokio::test]
async fn replay_reports_what_the_rules_would_have_said() {
    let f = fixture("replay").await;
    let out = run(
        &f,
        &["replay", "--garden", &f.garden.to_string(), "--days", "45"],
    );

    assert!(out.contains("Kitchen"));
    assert!(out.contains("first raised"), "{out}");
    assert!(out.contains("tasks over"), "{out}");
    // No sensor ever reported, and a replay that quietly hid that would be measuring
    // the gap rather than the garden.
    assert!(out.contains("no sensor reading"), "{out}");
}

#[tokio::test]
async fn replay_rejects_a_capability_it_does_not_know() {
    let f = fixture("badcap").await;
    let err = run_failing(
        &f,
        &[
            "replay",
            "--garden",
            &f.garden.to_string(),
            "--capability",
            "telepathy",
        ],
    );
    assert!(err.contains("not a capability"), "{err}");
}

#[tokio::test]
async fn a_bad_garden_id_explains_how_to_find_a_good_one() {
    let f = fixture("badid").await;
    let err = run_failing(&f, &["tank", "show", "--garden", "kitchen"]);
    assert!(err.contains("garden-cli gardens"), "{err}");
}

#[tokio::test]
async fn the_vision_flow_runs_from_init_to_apply() {
    let f = fixture("vision").await;
    let garden = f.garden.to_string();
    let map = f.dir.join("rois.json");
    let map_arg = map.display().to_string();

    let init = run(
        &f,
        &[
            "vision", "init", "--garden", &garden, "--width", "320", "--height", "480", "--out",
            &map_arg,
        ],
    );
    assert!(init.contains("16 slots"), "{init}");
    assert!(init.contains("will not be right"), "must warn: {init}");

    // A frame to check the rectangles against.
    let frame = f.dir.join("frame.png");
    image::RgbImage::from_pixel(320, 480, image::Rgb([120, 118, 122]))
        .save(&frame)
        .unwrap();
    let preview_out = f.dir.join("preview.png");
    let preview = run(
        &f,
        &[
            "vision",
            "preview",
            "--map",
            &map_arg,
            "--frame",
            &frame.display().to_string(),
            "--out",
            &preview_out.display().to_string(),
        ],
    );
    assert!(preview.contains("wrote"), "{preview}");
    assert!(preview_out.exists(), "the overlay should be written");

    // Before scaling, the tool has to admit the areas are not real.
    let untested = run(
        &f,
        &[
            "vision",
            "test",
            "--map",
            &map_arg,
            "--frame",
            &frame.display().to_string(),
        ],
    );
    assert!(untested.contains("placeholder units"), "{untested}");

    run(&f, &["vision", "scale", "--map", &map_arg, "--cm", "7", "--px", "70"]);
    let scaled = run(
        &f,
        &[
            "vision",
            "test",
            "--map",
            &map_arg,
            "--frame",
            &frame.display().to_string(),
        ],
    );
    assert!(!scaled.contains("placeholder units"), "{scaled}");

    let applied = run(&f, &["vision", "apply", "--garden", &garden, "--map", &map_arg]);
    assert!(applied.contains("real cm²"), "{applied}");
    assert!(run(&f, &["gardens"]).contains("vision:on"));

    run(&f, &["vision", "clear", "--garden", &garden]);
    assert!(run(&f, &["gardens"]).contains("vision:off"));
}

#[tokio::test]
async fn backup_writes_a_restorable_database() {
    let f = fixture("backup").await;
    let out = f.dir.join("backup.db");
    run(&f, &["backup", "--out", &out.display().to_string()]);
    assert!(out.exists());

    // The copy must be usable, not merely present — that is the whole difference
    // between `VACUUM INTO` and copying the file out from under a live WAL.
    let url = format!("sqlite://{}", out.display().to_string().replace('\\', "/"));
    let restored = Store::open_with(&url, f.dir.join("frames2")).await.unwrap();
    assert_eq!(restored.all_gardens().await.unwrap().len(), 1);
}

#[test]
fn tank_calibration_needs_no_database_at_all() {
    // Deliberate: you work out your tank constants standing next to the device with a
    // jug, which may well not be where the server is.
    let output = Command::new(env!("CARGO_BIN_EXE_garden-cli"))
        .args(["tank", "calibrate", "330:0", "240:5", "150:10", "60:15"])
        .env("GARDEN_DB", "sqlite:///nonexistent/path/nope.db")
        .output()
        .expect("run garden-cli");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("empty_distance_mm: 330.0"), "{stdout}");
    assert!(stdout.contains("worst residual:    0.00 L"), "{stdout}");
}

#[test]
fn a_sensor_wired_backwards_is_refused_with_an_explanation() {
    let output = Command::new(env!("CARGO_BIN_EXE_garden-cli"))
        .args(["tank", "calibrate", "60:0", "330:15"])
        .output()
        .expect("run garden-cli");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("measures down to the water"), "{stderr}");
}

#[test]
fn a_malformed_sample_says_what_the_format_is() {
    let output = Command::new(env!("CARGO_BIN_EXE_garden-cli"))
        .args(["tank", "calibrate", "330-0"])
        .output()
        .expect("run garden-cli");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("distance_mm:volume_l"), "{stderr}");
}

#[tokio::test]
async fn the_schedule_can_be_set_previewed_and_cleared() {
    let f = fixture("schedule").await;
    let garden = f.garden.to_string();

    let empty = run(&f, &["schedule", "show", "--garden", &garden]);
    assert!(empty.contains("no schedule set"), "{empty}");
    // Clearing must never read as "turn the lights off".
    assert!(empty.contains("does not mean dark"), "{empty}");

    let set = run(
        &f,
        &[
            "schedule", "set", "--garden", &garden, "--hours", "14", "--duty", "0.8",
        ],
    );
    assert!(set.contains("14.0 h at 80%"), "{set}");
    assert!(set.contains("--own-actuators"), "must say it is inert: {set}");

    let shown = run(&f, &["schedule", "show", "--garden", &garden]);
    assert!(shown.contains("14.0 h at 80%"), "{shown}");

    // Changing it reports the change in daily light, not just the new hours.
    let changed = run(
        &f,
        &[
            "schedule", "set", "--garden", &garden, "--hours", "10", "--duty", "0.8",
        ],
    );
    assert!(changed.contains("daily light"), "{changed}");

    let preview = run(&f, &["schedule", "preview", "--garden", &garden]);
    assert!(preview.contains("hour   light   pump"), "{preview}");
    assert_eq!(preview.lines().count(), 25, "a header and 24 hours");

    run(&f, &["schedule", "clear", "--garden", &garden]);
    assert!(run(&f, &["schedule", "show", "--garden", &garden]).contains("no schedule set"));
}

#[tokio::test]
async fn a_schedule_that_would_overload_the_supply_is_refused() {
    // The pump ceiling, enforced at the point a person could type past it.
    let f = fixture("overload").await;
    let err = run_failing(
        &f,
        &[
            "schedule",
            "set",
            "--garden",
            &f.garden.to_string(),
            "--pump-duty",
            "0.9",
        ],
    );
    assert!(err.contains("ceiling"), "{err}");
}

#[tokio::test]
async fn plan_suggests_something_for_every_empty_slot() {
    let f = fixture("plan").await;
    let out = run(&f, &["plan", "--garden", &f.garden.to_string()]);

    // Slot 0 is planted by the fixture; the other fifteen are not.
    assert!(out.contains("already coming"), "{out}");
    assert!(out.contains("harvest ~day"), "{out}");
    // And it explains the ranking, because "why this one" is the whole question.
    assert!(out.contains("same afternoon"), "{out}");

    let suggested_slots = out.lines().filter(|l| l.contains("light)")).count();
    assert_eq!(suggested_slots, 15, "one heading per empty slot:\n{out}");
}

#[tokio::test]
async fn plan_on_an_empty_tower_says_the_rhythm_is_yours_to_set() {
    let f = fixture("plan-empty").await;
    // Pull the only plant.
    run(&f, &["gardens"]);
    let store = Store::open_with(&f.url, f.dir.join("frames")).await.unwrap();
    store
        .remove_planting(f.garden, gardyn_planting_id(), Timestamp::now())
        .await
        .unwrap();
    drop(store);

    let out = run(&f, &["plan", "--garden", &f.garden.to_string()]);
    assert!(out.contains("sets the rhythm"), "{out}");
}

fn gardyn_planting_id() -> garden_core::PlantingId {
    garden_core::PlantingId(1)
}
