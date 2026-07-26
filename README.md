# gardyn

An autonomous management system for a Gardyn Studio 2 hydroponic garden, in Rust.

It reads the garden's sensors and camera, models what is growing in each of the 16
slots, and tells the operator what to do and when — water, feed, condition, prune,
harvest, clean — via self-hosted push, email, and a calendar feed. See
[DESIGN.md](DESIGN.md) for the architecture and the hardware plan.

## Status

Phase 0 (hardware recon) has not started. What exists today is the part that needs no
hardware: the domain model, the rule engine, and a simulator good enough to run a
season in milliseconds.

```
crates/
  gardyn-core     domain types, zero I/O          — 55 tests
  gardyn-hal      hardware traits + failsafes     —  5 tests
  gardyn-rules    20 rules + capability engine    — 84 tests
  gardyn-sim      physics model + season runner   — 32 tests
```

## Try it

```sh
cargo test --workspace
cargo run -p gardyn-sim            # 120-day season, or pass a day count
```

The simulation reports what the rule set achieves against operators of varying
diligence, and what each piece of optional hardware is worth:

```
configuration        harvest    canopy interrupts  dry days    tasks
--------------------------------------------------------------------
stock                   6285      2458        1.2         0      340
+ water temp            6285      2458        1.2         0      340
+ canopy vision         8327      2371        1.4         0      336
+ EC probe              9172      2303        2.8         0      326
```

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
