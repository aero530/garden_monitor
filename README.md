# gardyn

An autonomous management system for a Gardyn Studio 2 hydroponic garden, in Rust.

It reads the garden's sensors and camera, models what is growing in each of the 16
slots, and tells the operator what to do and when — water, feed, condition, prune,
harvest, clean — via self-hosted push, email, and a calendar feed. See
[DESIGN.md](DESIGN.md) for the architecture and the hardware plan.

## Status

Phase 0 (hardware recon) has not started. What exists today is everything that needs
no hardware: the domain model, the rule engine, a simulator good enough to run a
season in milliseconds, and a multi-user web application.

```mermaid
flowchart TD
  core["<b>gardyn-core</b><br/>domain types, zero I/O"]
  hal["<b>gardyn-hal</b><br/>sensor + actuator traits"]
  rules["<b>gardyn-rules</b><br/>21 rules + capability engine"]
  auth["<b>gardyn-auth</b><br/>accounts, roles, sharing"]
  proto["<b>gardyn-proto</b><br/>edge ↔ brain wire format"]
  notify["<b>gardyn-notify</b><br/>ntfy · SMTP · iCal"]
  sim["<b>gardyn-sim</b><br/>physics + season runner"]
  store["<b>gardyn-store</b><br/>SQLite + frame files"]
  web["<b>gardyn-web</b><br/>the brain"]
  edge["<b>gardyn-edge</b><br/>the Pi agent"]
  guard["<b>gardyn-guard</b><br/>failsafe supervisor"]

  core --> hal & rules & auth & proto & notify
  hal --> sim & edge & guard
  rules --> sim & store & web
  auth --> store & web
  notify --> store & web
  proto --> edge & web
  store --> web
  sim --> web
  core --> web & edge

  classDef bin fill:#2f7d4f22,stroke:#2f7d4f,stroke-width:2px
  class web,edge,guard,sim bin
```

Green outlines are binaries. Each crate has its own README with a fuller diagram,
worked examples, and — for the hardware crates — pinouts.

| Crate | | Tests |
|---|---|---|
| [gardyn-core](crates/gardyn-core/) | domain types, 135-variety plant book, zero I/O | 79 |
| [gardyn-hal](crates/gardyn-hal/) | hardware traits + duty-cycle failsafes | 5 |
| [gardyn-proto](crates/gardyn-proto/) | wire format shared by agent and brain | 12 |
| [gardyn-rules](crates/gardyn-rules/) | 21 rules + capability engine | 87 |
| [gardyn-auth](crates/gardyn-auth/) | accounts, roles, sharing, signed links | 78 |
| [gardyn-notify](crates/gardyn-notify/) | ntfy, SMTP, iCal + delivery policy | 44 |
| [gardyn-store](crates/gardyn-store/) | SQLite persistence + frame storage | 107 |
| [gardyn-sim](crates/gardyn-sim/) | physics model + season runner | 32 |
| [gardyn-web](crates/gardyn-web/) | axum + maud application, agent API | 46 |
| [gardyn-edge](crates/gardyn-edge/) | Pi agent: recon, telemetry, camera | 22 |
| [gardyn-guard](crates/gardyn-guard/) | heartbeat supervisor, failsafe | 7 |

## Plantings

`/gardens/{id}/slots` is where you record what went where. Plant a slot, mark it
sprouted, log a thin / prune / root check / harvest, or pull the plant — the row stays
for yield history and the slot frees up.

**This is what makes the system useful before any hardware work.** Thinning windows,
harvest dates, root-check cadence and end-of-life replanting all derive from the
variety book and a planting date, so a Studio with no agent, no probes and no camera
still gets real advice. Sensors upgrade that advice; they are not a precondition for it.

Two things worth knowing:

- **Completing a task writes back to the plant.** The rule engine is stateless and
  re-derives from stored state, so ticking "prune roots" has to move
  `last_root_check` or the identical task reappears on the next evaluation. Marking it
  done without that would look like it worked and then silently undo itself.
- **A slot holds at most one living plant, enforced by a partial unique index**
  (`WHERE removed_at IS NULL`) rather than a check-then-insert in Rust. Two people
  tending a shared garden can submit "plant slot 3" at the same moment.

## Try it

```sh
cargo test --workspace
cargo run -p gardyn-sim            # 120-day season, or pass a day count
```

### Run the web app

```sh
GARDYN_INSECURE_COOKIES=1 \
GARDYN_AGENT_TOKEN=$(openssl rand -hex 32) \
cargo run -p gardyn-web
```

Open <http://localhost:8080>. The first account to register becomes the server
administrator; after that, registration is closed and new people join through an
invitation link. Add a garden with model **Simulated** to see the dashboard, rules,
and task lifecycle working without any hardware.

| Variable | Default | |
|---|---|---|
| `GARDYN_DB` | `sqlite://gardyn.db` | |
| `GARDYN_DATA_DIR` | `gardyn-data` | camera frames land in `$GARDYN_DATA_DIR/frames` |
| `GARDYN_BIND` | `0.0.0.0:8080` | |
| `GARDYN_BASE_URL` | `http://$GARDYN_BIND` | used to build invite links |
| `GARDYN_AGENT_TOKEN` | *unset* | agent API is **closed** when unset |
| `GARDYN_INSECURE_COOKIES` | *unset* | set only for plain-HTTP development |

## Accounts and sharing

One account holds any number of gardens, and any garden can be shared with other
accounts at a role:

| Role | Can |
|---|---|
| **Viewer** | see the garden and its history |
| **Caretaker** | + complete tasks, log actions, manage plantings |
| **Steward** | + configure the garden, control hardware, invite people |
| **Owner** | + delete and transfer |

Properties the tests enforce rather than merely document:

- **Nobody can grant their own role**, and ownership never moves by invitation — so a
  shared garden cannot become an unbounded privilege chain the owner never approved.
- **A garden you are not a member of returns 404, not 403.** Garden ids appear in
  URLs; a "Forbidden" would confirm the id is real and turn guessing into enumeration.
- **A server administrator gets the system view and nothing else.** They can see that
  a Pi is offline; they cannot see what you are growing. `require_admin` and `require`
  are separate questions and never consult each other.
- **An invitation is bound to its recipient**, so a forwarded link does not work for
  whoever opens it first — and it cannot be used to register under a different address
  on a server with sign-ups closed.
- **Secrets are stored as digests.** Session cookies, invite links, and one-tap
  notification links all hash before storage, so a leaked backup yields nothing usable.

## Camera

Frames are indexed in SQLite and stored as files under `$GARDYN_DATA_DIR/frames`.
Blobs stay out of the database deliberately: one frame an hour is ~8,700 images a year
per garden, which would bloat every backup and every `VACUUM INTO`.

An agent posts the raw image with metadata in headers — no multipart to assemble on a
Pi Zero:

```sh
curl -X POST "$BASE/api/gardens/$GARDEN_ID/frames" \
  -H "Authorization: Bearer $GARDYN_AGENT_TOKEN" \
  -H 'X-Width: 1920' -H 'X-Height: 1080' \
  -H 'X-Light-Duty-Milli: 800' -H 'X-Photo-Mode: true' \
  --data-binary @frame.jpg
```

A garden with model **Simulated** renders its own frames from the physics model —
one blob per occupied slot, sized by canopy area and tinted by chlorosis index. Not a
photograph of a plant, and not pretending to be: its job is to make capture, storage,
authorization, and display real so that swapping in `/dev/video0` changes one function.

Four things this gets right that a static file mount would not:

- **Images go through the same membership check as everything else.** A photograph of
  someone's kitchen is at least as sensitive as the sensor readings beside it.
- **Frame lookups are scoped to the garden in the SQL**, not checked afterwards — so a
  frame id from one garden cannot be fetched through another garden's URL, even by
  someone who legitimately belongs to that other one.
- **Uploaded bytes are sniffed, never trusted.** An agent claiming `image/jpeg` while
  sending HTML would otherwise get its content served back from our origin. Responses
  also pin the content type and set `nosniff`.
- **Deleting a garden deletes the photographs from disk.** Foreign keys cascade the
  rows; the bytes are the database's blind spot, and leaving pictures of someone's home
  behind after they deleted the garden is not acceptable.

`X-Photo-Mode` matters more than it looks. Under the Studio 2's sunrise/sunset ramp,
brightness varies with capture time, so colour comparisons between frames measure the
clock rather than the plant. Frames captured at the pinned reference level are marked
comparable; the rest are badged **ambient** in the UI and should be kept out of any
colour trend.

The simulation reports what the rule set achieves against operators of varying
diligence, and what each piece of optional hardware is worth:

| Configuration | Harvest | Canopy | Interrupts | Dry days | Tasks |
|---|---:|---:|---:|---:|---:|
| stock | 6285 | 2458 | 1.2 | 0 | 340 |
| + water temp | 6285 | 2458 | 1.2 | 0 | 340 |
| + canopy vision | **8327** | 2371 | 1.4 | 0 | 336 |
| + EC probe | **9172** | 2303 | 2.8 | 0 | 326 |

Read that as: the $5 water probe buys reasoning, not yield; canopy vision is worth
about a third more harvest; and the EC probe adds another 10% at the cost of waking
you up more than twice as often.

## The central idea: capabilities

Every optional thing — deferred probes, each vision stage, actuator ownership after
firmware takeover — is one mechanism. A rule declares what it needs and how
authoritative it is; the engine runs only rules whose hardware is present, and for
each task kind runs only the best-informed one.

```rust
trait Rule {
    fn requires(&self)   -> &'static [Capability];
    fn produces(&self)   -> &'static [TaskKind];
    fn precedence(&self) -> u8;
    fn evaluate(&self, state: &GardenState) -> Vec<Task>;
}
```

Fit an EC probe and `plant-food-by-volume` stands down in favour of
`plant-food-by-ec`. No code change, no config migration. Lose the probe mid-season and
the fallback resumes on the next tick — which is why capabilities are runtime state
rather than Cargo features.

**The rule that makes this safe:** a higher-precedence rule wins the whole task kind,
so it must be a *superset* of the one it displaces. Every measured rule here keeps the
calendar logic and uses its sensor to fire early or escalate, rather than replacing it.
`adding_a_capability_never_leaves_a_task_kind_uncovered` in `gardyn-rules` enforces it.

## Design notes worth knowing

- **Rules are stateless.** They emit what should be outstanding *now*, keyed by
  `TaskKey`. Completion, snoozing, and escalation live in the brain, not the rules.
  This is what makes it possible to replay recorded history against a modified rule.
- **Every task carries a `rationale`.** "Why am I being told this?" always has an
  answer: *"tank at 22%, using 0.5 L/day, reserve reached in 1.8 days."*
- **The pump is a sensor.** INA219 current draw against a clean baseline detects flow
  restriction, turning "prune roots" and "clean" into measured triggers.
- **`Duty::pump()` clamps to 30%** in the type. After firmware takeover there is no
  vendor firmware left to catch an over-current mistake.
- **Nothing here is self-hosted-hostile.** No third-party service sits in the runtime
  path; `VisualDiagnosis` targets a local VLM rather than a hosted API.

## Calibration

Several constants are placeholders, marked as such in the source: tank calibration
distances, dosing rates, the EC-per-millilitre conversion, and the growth
coefficients. They are structured as data so that correcting them against the real
device is a value change, not a code change.

The slot geometry (2 columns × 8 rows) and the peripheral map in DESIGN.md come from
community work on Gardyn Home 3.0/4.0. **Studio 2 internals are unverified.** Phase 0
confirms them.
