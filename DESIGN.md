# Gardyn Studio 2 Autonomous Management System — Design

**Status:** Draft for review · **Date:** 2026-07-26

A Rust system that owns a Gardyn Studio 2 end to end: reads its sensors and camera,
controls its lights and pump, models what is growing in each of the 16 slots, and
tells the operator what to do and when — via push, email, and calendar — without
the operator having to remember to check anything.

---

## 1. Locked decisions

| Decision | Choice |
|---|---|
| Device | Gardyn Studio 2 (Gen 2, launched Oct 2025) |
| Hardware scope | **Full firmware takeover** — we own lights, pump, camera, sensors |
| Brain host | Fedora 44 VM on Proxmox |
| Notification channels | ntfy push, email (SMTP), iCal feed. **No SMS.** |
| Hosting constraint | **Everything self-hosted.** No third-party SaaS in the runtime path. |
| Language | Rust throughout |

### Self-hosting consequences

- **ntfy** runs as our own container, not `ntfy.sh`. The phone app points at our server.
- **Vision Phase C** cannot use the Claude API. It uses a **local VLM** (Ollama serving
  Qwen2.5-VL or similar) on the Fedora VM, behind a trait so the backend is swappable.
- **Email** is the awkward case: outbound SMTP from a residential IP is widely rejected
  on reputation. The SMTP endpoint is pure configuration, so it works with a
  self-hosted Postfix/mailcow. **ntfy is the reliable channel; email is best-effort.**

Because SMS is out, the top escalation tier becomes **ntfy priority 5 (`max`)**, which
bypasses Do Not Disturb on both iOS and Android. That covers the "tank is dry in 12
hours" case without a Twilio bill.

---

## 2. What we know about the hardware

Confirmed for Gardyn Home 3.0/4.0 via the community project
[`iot-root/garden-of-eden`](https://github.com/iot-root/garden-of-eden). Gardyn devices
run **Raspberry Pi OS on a Pi Zero–class board**, uplinked to Azure IoT Hub.

| Peripheral | Part | Interface |
|---|---|---|
| Air temp / humidity | AM2320 | I²C `0x38` |
| Pump current | INA219 | I²C `0x40` |
| PCB temp | PCT2075 | I²C `0x48` |
| Water level | DYP-A01-V2.0 ultrasonic | GPIO19 trig / GPIO26 echo |
| Grow lights | LED full spectrum | PWM GPIO18 @ 8 kHz |
| Pump | — | PWM GPIO24 @ 50 Hz, **30% max duty** |
| Cameras | USB UVC | `/dev/video0`, `/dev/video1` |

### Studio 2 deltas (from product documentation)

- **One** ultra-wide HD camera on the light bar, not two.
- 16 slots, 4+ gal tank, 1.4 sq ft footprint.
- Sunrise/Sunset lighting mode — gradual PWM ramps, not a square wave.
- "No-Clean Columns" — sealed silicone modules that suppress buildup and
  crystallization. Reduces cleaning frequency; the cleaning rules should weight
  measured signals over the calendar accordingly.

### ⚠ Unverified

**The peripheral table above is from Home 3.0/4.0. Studio 2 internals are
undocumented publicly.** Phase 0 is a blocking prerequisite for any edge work.
Studio 2 may use a different SoC entirely (Pi Zero 2 W, CM4, or non-Pi), and may
use eMMC rather than a removable SD card.

### Security background

Gardyn patched default-SSH-credentials (CVE-2025-29629) and command injection
(CVE-2025-29631) in firmware ≥ `master.627`, with a further round in 2026
(CVE-2026-13768 et al.). **We do not use these.** On hardware you own, the clean
path is physical: pull the storage, image it, add your own key.

---

## 3. What's missing, and why it matters

The stock sensor set has no **EC/TDS**, no **pH**, and no **water temperature**.
Without EC, "add plant food" can only ever be a calendar estimate. These three
probes are what convert this project from a reminder app into a control system.

| Probe | Part | Interface | Cost | Status |
|---|---|---|---|---|
| Water temperature | DS18B20 waterproof | 1-Wire | ~$5 | **Committed** |
| EC / TDS | DFRobot Gravity EC or Atlas EZO-EC | ADS1115 ADC / I²C | $70–170 | Deferred |
| pH | DFRobot Gravity pH or Atlas EZO-pH | ADS1115 ADC / I²C | $50–170 | Deferred |

Water temperature ships in the base build — it drives dissolved oxygen and root-rot
risk, and it costs five dollars.

**EC and pH are deferred hardware.** The software treats them as optional
capabilities that light up when the probes appear (see §7.1). Nothing needs
rewriting to enable them — the rules that depend on EC are simply inert until an EC
reading exists, and the calendar-estimate fallbacks stand down automatically when it
does.

**I²C address collision, for when they are added:** the ADS1115 defaults to `0x48`,
already taken by the PCT2075. Strap `ADDR` to VDD for `0x49`.

### The pump is already a sensor

The INA219 on the pump is underrated. Its current draw profile is a **flow
restriction proxy**: rising steady-state current or a changed startup transient means
clogged roots or biofilm. That converts "prune roots" and "clean" from monthly
calendar entries into measured triggers.

---

## 4. Architecture

```
┌─ Gardyn Studio 2 (Pi) ─────────┐        ┌─ Fedora 44 VM on Proxmox ──────────────┐
│                                │        │                                        │
│  gardyn-edge     (Rust)        │  MQTT  │  mosquitto        (container)          │
│   · sensor polling             ├───────►│  gardyn-brain     (container)          │
│   · camera capture             │        │   · ingest → SQLite                    │
│   · PWM light + pump control   │◄───────┤   · state estimation                   │
│   · offline ring buffer (redb) │schedule│   · rules engine → Tasks               │
│                                │ updates│   · vision pipeline                    │
│  gardyn-guard    (Rust)        │        │   · scheduler → ntfy / SMTP / iCal     │
│   · heartbeat watchdog         │        │   · axum + HTMX dashboard              │
│   · failsafe PWM takeover      │        │                                        │
│                                │        │  Tailscale — phone reaches ack links   │
│  bcm2835_wdt (hardware)        │        │  off-LAN without exposing the VM       │
└────────────────────────────────┘        └────────────────────────────────────────┘
```

### The load-bearing rule

**The brain is never in the water or light control loop.** The Pi holds a resident
schedule and executes it autonomously. The brain pushes *schedule updates* and
*reads telemetry* — it never issues per-cycle commands. If the LAN dies, the VM
dies, or Proxmox reboots for a kernel update, the garden keeps growing on its
last-known-good schedule. This is non-negotiable given full takeover.

---

## 5. Safety model for full takeover

Owning the firmware means owning the failure modes. Four layers:

1. **Physical rollback.** Work on a *cloned* SD card. The original stays in a drawer,
   untouched. Rollback is a two-minute card swap, not a reflash.
2. **Boot-time safe defaults.** `gardyn-failsafe.service` runs before `gardyn-edge`
   and applies a conservative schedule (14h light / 10h dark, pump 15 min on /
   45 min off at 25% duty). Even if the main agent never starts, plants get light
   and water.
3. **Heartbeat supervisor.** `gardyn-guard` is a tiny separate process with minimal
   dependencies watching a heartbeat file. If `gardyn-edge` stops beating for N
   minutes, guard seizes the PWM lines and applies the safe schedule. Two processes
   means a panic in the complex one can't take out the simple one.
4. **Hardware watchdog.** `bcm2835_wdt` plus systemd `RuntimeWatchdogSec` reboots a
   hung kernel, which lands back in layer 2.

Never exceed **30% pump duty** — garden-of-eden notes full-on likely exceeds the
supply's current budget.

### Cutting the cloud is now a feature

Full takeover means dropping the Azure IoT Hub uplink. That loses Kelby and app
support, but it also stops OTA firmware pushes from clobbering our work. Given the
CVE history, removing the cloud attack surface is a security improvement.

---

## 6. Phase 0 — recon (blocking)

Nothing gets built against Studio 2 hardware until this is answered.

**Access:** power down, remove storage, image it on the Fedora box
(`dd` → keep the `.img`), then on a *fresh card* enable SSH by touching `ssh` in the
boot partition and adding a pubkey to the user's `authorized_keys`.
If Studio 2 uses eMMC or a CM4, fall back to UART console or `rpiboot`.

**Inventory to capture:**

```sh
cat /proc/device-tree/model; cat /proc/cpuinfo      # SoC and board
cat /etc/os-release; uname -a                        # OS and kernel
systemctl list-units --type=service --state=running  # factory services
i2cdetect -y 1                                       # confirm 0x38 / 0x40 / 0x48
raspi-gpio funcs                                     # GPIO allocation, free pins
v4l2-ctl --list-devices; v4l2-ctl --list-formats-ext # camera capability
ls /sys/bus/w1/devices 2>/dev/null                   # 1-Wire for DS18B20
pigs gdc 18; pigs gdc 24                             # live PWM duty, if pigpiod
```

**Parity capture — do not skip this.** Before disabling anything, run a
**read-only** `gardyn-edge` alongside the factory firmware for 1–2 weeks, sampling
PWM duty on GPIO18/24 at 1 Hz. That gives ground truth for the stock light curve
(including the sunrise/sunset ramp) and pump duty cycle. Replicate that baseline
first, then improve on it. Full takeover *requires* the read-only phase rather than
skipping it.

If the factory code uses pigpiod, `pigs gdc <pin>` reads duty directly. Otherwise
try `/sys/class/pwm/pwmchip0/pwm0/{duty_cycle,period}`, or jumper a spare GPIO
input to the PWM line and sample it.

---

## 7. Domain model

```rust
Device   { tank_geometry, slots: [Slot; 16] }
Slot     { position, row, column }        // row matters: light and flow vary by height
Planting { slot, variety, planted_at, germinated_at, thinned_at,
           stage, last_root_check, last_harvest, expected_eol }
Variety  { germination_days, days_to_harvest, productive_life, canopy_class,
           needs_pruning, needs_pollination, ec_target, ph_target }
TankEvent{ TopOff { liters }, Refresh, FoodDose { ml }, HydroBoost, DeepClean }
Observation { ts, kind, value }
Task     { kind, slot, due_window, severity, rationale, state }
```

Rules are **pure functions** `fn(&GardenState) -> Vec<Task>`. That makes them
unit-testable, and it lets you replay months of recorded history against a modified
rule to see what it *would* have said. Every `Task` carries a `rationale` string, so
"why am I being told this?" always has an answer: *"water at 22%, consuming 0.5 L/day,
empty in 1.8 days."*

### 7.1 Capability model — the spine of optional features

Every optional thing in this system — deferred probes, each vision phase, actuator
ownership — is modelled as a `Capability`. This is one mechanism, not three.

```rust
enum Capability {
    // base sensors
    AirTemperature, AirHumidity, WaterLevel, PumpCurrent, PcbTemperature,
    WaterTemperature,        // committed: DS18B20
    // deferred hardware
    Conductivity, PotentialHydrogen,
    // vision, independently switchable
    CanopyMetrics,           // phase A — HSV masking, no ML
    PlantSegmentation,       // phase B — ONNX
    VisualDiagnosis,         // phase C — local VLM
    // actuators, arrive at takeover
    LightControl, PumpControl,
}
```

Each rule declares what it needs and how authoritative it is:

```rust
trait Rule {
    fn requires(&self) -> &'static [Capability];
    fn produces(&self)  -> &'static [TaskKind];
    fn precedence(&self) -> u8;
    fn evaluate(&self, state: &GardenState) -> Vec<Task>;
}
```

The engine keeps only rules whose requirements are satisfied, then — for each
`TaskKind` — runs only the **highest-precedence surviving rule**. That gives graceful
degradation for free:

| TaskKind | High precedence (needs `Conductivity`) | Fallback (base sensors only) |
|---|---|---|
| `AddPlantFood` | dose from measured EC vs variety target | dose ∝ litres added since last dose |
| `PruneRoots` | pump-current restriction trend | fixed 2–4 week cadence |
| `Harvest` | canopy area vs variety threshold | days-to-harvest from the variety book |

Plug in an EC probe and the estimate-based rule stands down silently, replaced by the
measured one. No code changes, no config migration. The same holds for each vision
phase — enable `CanopyMetrics` and the harvest rule upgrades from calendar to
measurement.

**Capabilities are runtime state, not compile-time features.** A probe that fails
mid-season drops its capability, and the fallback rule resumes on the next tick. This
is why they are not Cargo features.

---

## 8. Rules catalog

From the documented Gardyn care cycle — top off weekly, refresh tank monthly,
HydroBoost with every top-off and refresh, root check every 2–4 weeks, thin during
weeks 2–6, deep clean annually — plus sensor-derived triggers.

| Task | Calendar trigger | Measured trigger |
|---|---|---|
| Add water | — | level + consumption rate → forecast; computes *how much* |
| Add plant food | dose ∝ liters added; half-strength pre-germination | EC below variety target |
| Add conditioner | every top-off / refresh | algae pixels, pump current drift |
| Prune roots | every 2–4 weeks per planting | pump current ↑, consumption ↓ |
| Prune plants | variety flag + age | canopy area threshold, neighbor shading |
| Harvest | days-to-harvest, cut-and-come-again cadence | canopy area, bolting risk from heat |
| Clean / refresh | monthly; annual deep clean | pump profile, biofilm (weighted down for No-Clean Columns) |
| Thin | weeks 2–6 → 1/yCube, 3 for herbs | seedling count from vision |
| Pollinate | fruiting varieties in flower | flower detection |
| Replant | end of productive life | growth curve plateau |

**Daily water consumption is a whole-garden health proxy.** It is aggregate
transpiration. A sudden drop means something is wrong before any plant looks wrong.

### Succession planner

Reactive rules keep plants alive; they don't optimize production. A planner assigns
varieties to slots over time so harvests stagger and no slot sits idle, respecting
slot position (light and flow vary by row), variety lifespan, and stated
preferences. Greedy heuristic first; treat as an optimization problem later.

---

## 9. Vision pipeline

One ultra-wide camera, 16 slots, one frame → 16 ROIs.

**Undistort first.** An ultra-wide lens has significant barrel distortion — edge
slots will measure smaller than center slots if you skip this. Calibrate once with a
checkerboard, store the camera matrix and distortion coefficients, undistort before
extracting ROIs.

**Photo mode — the payoff for full takeover.** Because we own the light PWM, we can
briefly set the lights to a known fixed duty, let them settle, capture, then restore.
Every frame is then photometrically comparable. Under the stock sunrise/sunset ramp
this is impossible, and color-based diagnosis (yellowing → nitrogen) is invalid.
This alone justifies the takeover.

### Three independent, individually switchable features

Each stage is a separate `Capability` and a separate module. They are **not** a
dependency chain you must climb — any subset can run. Undistortion and ROI extraction
are shared plumbing that sits below all three.

| Capability | Method | Cost to run | Adds |
|---|---|---|---|
| `CanopyMetrics` | HSV green masking per ROI, no ML | negligible | canopy area, colour stats, growth curves, chlorosis, stalled growth |
| `PlantSegmentation` | ONNX model via `ort` | moderate CPU | per-plant masks, seedling counts for thinning, flower/fruit detection |
| `VisualDiagnosis` | **local VLM** (Ollama, Qwen2.5-VL) | heavy, run sparingly | qualitative "what's wrong with this plant", plain-language daily brief |

`CanopyMetrics` alone is roughly 80% of the value and is the default. The other two
can be toggled on independently at any time, and each one silently upgrades the rules
that declare it (see §7.1).

**Self-hosted constraint:** `VisualDiagnosis` runs against a local Ollama endpoint,
not a hosted API. It sits behind a `DiagnosisBackend` trait, so the backend is
swappable without touching the rules.

**The VLM is strictly advisory.** Deterministic rules own anything touching dosing,
water, or actuators. A model that hallucinates a nutrient deficiency must never be
able to dose the tank.

---

## 10. Notifications

The "I don't want to have to remember" requirement is doing most of the work here.

1. **Escalation ladder** — silent → ntfy default → ntfy + email → ntfy priority 5
   (`max`, bypasses DND). Escalates on overdue and on severity.
2. **One-tap acknowledgment** — every notification carries signed Done / Snooze /
   Not-Applicable links requiring no login. Completion feeds state, because "when did
   I last dose HydroBoost" must be *known*, not remembered.
3. **Auto-verification** — you tap "added water"; if the level sensor doesn't move
   within minutes, the task silently un-completes and re-fires. This is the mechanism
   that makes the system trustworthy rather than merely noisy.
4. **Batching and quiet hours** — one morning brief with the day's actions;
   interrupts reserved for urgent items. The iCal feed carries scheduled work (tank
   refresh Saturday) into your existing calendar.

**Ack links must work off-LAN.** Tailscale on the phone and the VM is the clean
answer — no inbound exposure. Cloudflare Tunnel scoped to the ack endpoint is the
alternative.

---

## 11. Deployment — Fedora 44 on Proxmox

**VM:** 2 vCPU / 4 GB RAM / 40 GB disk is ample.

**Podman + Quadlet**, not Docker Compose — it's the Fedora-native path and gives real
systemd units. Drop `.container` files in `/etc/containers/systemd/`.

Containers: `mosquitto` (broker), `ntfy` (self-hosted push), `gardyn-brain`, and
`ollama` only if `VisualDiagnosis` is enabled. Grafana + VictoriaMetrics are a
worthwhile later add for time-series exploration; the built-in dashboard covers the
operational view.

All of these are self-hosted by design — no third-party service sits in the runtime
path. The phone's ntfy app is pointed at our server rather than `ntfy.sh`.

- **SELinux** is enforcing — volume mounts need `:Z`. Bind unprivileged ports
  (1883, 8080).
- **firewalld** — 1883 restricted to the LAN, 8080 to Tailscale.
- **SQLite in WAL mode.** A Proxmox snapshot of a live SQLite file isn't guaranteed
  consistent; schedule `VACUUM INTO` to a backup file and snapshot that.

**Cross-compiling for the Pi:** if Studio 2 is aarch64 (Pi Zero 2 W / CM4), the
target is tier 1 and trivial. If it's ARMv6 (Pi Zero v1), use `cross` — on Fedora set
`CROSS_CONTAINER_ENGINE=podman`. `cargo-zigbuild` is the fallback.

---

## 12. Crate layout

```
crates/
  gardyn-core/     domain types, zero I/O                      ✅ built
  gardyn-hal/      sensor/actuator traits + simulated impls    ✅ built
  gardyn-rules/    pure rule functions over GardenState        ✅ built
  gardyn-sim/      physics model + season runner               ✅ built
  gardyn-auth/     accounts, roles, sharing, sessions          ✅ built
  gardyn-store/    SQLite persistence                          ✅ built
  gardyn-web/      axum + maud application, fleet view         ✅ built
  gardyn-proto/    MQTT topics + payload schemas, shared edge↔brain
  gardyn-edge/     Pi binary: sensors, PWM, camera, MQTT
  gardyn-guard/    failsafe supervisor, minimal deps
  gardyn-vision/   frame → per-slot metrics
  gardyn-notify/   ntfy / SMTP / iCal adapters
  gardyn-cli/      calibrate, log events, replay history
data/varieties.json
deploy/            systemd units, Quadlet files, install.sh
```

### Multi-tenancy

The system is multi-user from the storage layer up: one account holds many gardens,
and any garden can be shared with other accounts at a role (viewer, caretaker,
steward, owner). See `gardyn-auth` for the policy and `gardyn-store/tests/tenancy.rs`
for the isolation guarantees exercised against a real database.

Two decisions worth recording:

- **`Actor` is the only authorization decision point.** Handlers never compare roles;
  they call `Actor::require`. One place to audit beats a check per handler.
- **Server administration and garden access are disjoint.** An admin sees fleet health
  and account counts, never garden contents. This is why `require_admin` does not fall
  through to `require`, and why the fleet page carries no per-garden data.

**Key crates.** Edge: `tokio`, `rppal` (GPIO/PWM/I²C, hardware PWM on GPIO18),
`ina219`, `am2320`, `nokhwa` (V4L2), `rumqttc`, `redb`.
Brain: `axum`, `sqlx`/SQLite, `maud` + HTMX (no JS build step), `image`, `ort`,
`lettre`, `icalendar`, `reqwest`, `tokio-cron-scheduler`.

Because `gardyn-hal` is trait-based, a `SimulatedGarden` backend runs the entire
brain, rules engine, vision pipeline, and UI **on a Windows dev box with no Pi in the
loop**. Most of the work in this project isn't hardware work, and it shouldn't be
gated on hardware.

---

## 13. Roadmap

| Phase | Deliverable | Gate |
|---|---|---|
| **0** | Recon: shell access, SD image backup, peripheral inventory | Studio 2 hardware confirmed |
| **1** | `gardyn-edge` read-only + parity capture of factory PWM | 1–2 weeks of baseline data |
| **2** | `gardyn-brain`: ingest, SQLite, state estimation, dashboard, slot/planting model | Water forecasting accurate |
| **3** | `gardyn-rules` + notifications (ntfy/email/iCal) + ack loop + auto-verify | Useful without vision |
| **4** | `gardyn-vision` `CanopyMetrics`: undistortion, ROI calibration, canopy tracking | Harvest prediction working |
| **5** | DS18B20 water temperature → root-zone rules | Probe reading reliably |
| **6** | **Takeover**: `gardyn-guard`, failsafe, cut cloud, own lights + pump, photo mode | Parity proven, rollback tested |
| **7** | Succession planner | — |
| *opt* | `PlantSegmentation` (ONNX) — enable any time after phase 4 | — |
| *opt* | `VisualDiagnosis` (local Ollama VLM) — enable any time after phase 4 | — |
| *opt* | EC + pH probes → measured dosing supersedes estimates | hardware purchased |

The three `opt` rows are deliberately unordered and unblocked. Because they are
capabilities rather than build stages (§7.1), each can be switched on independently
whenever the hardware or appetite appears.

Phase 6 is deliberately late. Everything valuable is reachable read-only; takeover is
what unlocks photometric consistency and custom light curves, and it should happen
once the rest is proven and a rollback has been rehearsed.

---

## 14. Risks

- **Studio 2 internals may differ materially** from the documented Home 3.0/4.0 map.
  Phase 0 could invalidate parts of Sections 2 and 4.
- **eMMC instead of SD** would make rollback much harder and changes the access path.
- **OTA firmware push** could clobber pre-takeover work. Idempotent install script;
  after takeover the cloud is cut, so this resolves itself.
- **Warranty** is almost certainly void once the storage is modified.
- **Agent failure kills plants.** Mitigated by the four-layer safety model — but the
  failsafe must be tested by deliberately killing `gardyn-edge` and confirming guard
  takes over, before Phase 6 goes live.
- **Pump over-duty** risks the power supply. Enforce the 30% cap in `gardyn-hal`, not
  in calling code.

---

## Sources

- [iot-root/garden-of-eden](https://github.com/iot-root/garden-of-eden) — peripheral map, GPIO/I²C details
- [CISA ICSA-26-055-03 — Gardyn Home Kit](https://www.cisa.gov/news-events/ics-advisories/icsa-26-055-03)
- [SecurityWeek — Critical Flaws Exposed Gardyn Smart Gardens](https://www.securityweek.com/critical-flaws-exposed-gardyn-smart-gardens-to-remote-hacking/)
- [Gardyn — Getting to Know Your Gardyn's Care Cycle](https://help.mygardyn.com/en/articles/1772865)
- [Gardyn — Tank Refresh Guide](https://help.mygardyn.com/en/articles/1788097)
- [Gardyn — How the Gardyn's Cameras Work](https://help.mygardyn.com/en/articles/1773313)
- [Gardyn Studio 2 product page](https://mygardyn.com/product/gardyn-studio-gen2/)
- [Gardyn — Security update](https://mygardyn.com/blog/security-update/)
