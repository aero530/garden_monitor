# Factory schedule — Gardyn Studio 2, profile `hw_gm20`

Captured 2026-08-30 from the stock firmware, image `master.634`, by reading
`/usr/local/etc/schedules/*.json` and `sensors/LED.py` rather than by sampling pins.
**This is the parity baseline DESIGN.md §6 exists to produce.** Phase 6 replicates it
first and improves on it second.

## Light

Identical on all seven days. Three levels, five transitions:

| Local time | Signal | Level |
|---|---|---|
| 00:00 | `off` | 0 % |
| 07:00 | `on` | 50 % |
| 08:00 | `boost` | 100 % |
| 21:00 | `on` | 50 % |
| 22:00 | `off` | 0 % |

**Photoperiod 15 h**: one hour at half output at each end, thirteen hours at full.
That pair of bookends *is* the "Sunrise/Sunset" feature — not a gradual dawn.

`on` is not a constant. It reads `/usr/local/etc/sensors/prev_light_status`, the level
last set from the app, defaulting to 50 % if unreadable. This unit holds **50**.
`/usr/local/etc/sensors/light_status` holds the current level; it read **100** during
the boost window, matching `gdc 18`.

### Drive

```
hardware_PWM(GPIO18, 20_000 Hz, percent * 10_000)     # range 1_000_000, linear
```

Transitions are smoothed over **1.5 s** in 100 steps, enabled because the `hw_gm20`
profile sets `withSunsetSunrise`:

```
alpha = x / 100                                       # 0→1 rising, 1→0 falling
value = start + (end - start) * 0.3 * alpha / (1.3 - alpha)
```

An ease-in curve: slow to leave the starting level, quick to arrive.

## Pump

Identical on all seven days. **Four five-minute runs, 20 minutes a day.**

| On | Off |
|---|---|
| 07:00 | 07:05 |
| 12:30 | 12:35 |
| 17:30 | 17:35 |
| 22:55 | 23:00 |

Driven as a plain digital output — `set_mode(24, OUTPUT)`, then `write(24, 1)` or
`write(24, 0)`. **No PWM, no duty cycle.** A guardian thread forces it off after
`MAX_WATER_TIME` = 15 minutes, three times the scheduled run.

The last run starts an hour after the lights go out.

## Also captured

| | |
|---|---|
| Profile | `hw_gm20` → GM2.0: one camera, `withSunsetSunrise`, **`rotatePhotos`**, `withRealtimeWaterLevel` |
| Services | `waterlevel,iot` — so `gy_wl` owns the tank sensor and `gy_iot` owns Azure; `main.py` only actuates |
| INA219 shunt | 0.08 Ω, sampled once a second for the duration of every pump run, reported as median watts and standard deviation |
| Pump current bound | 15 minutes of on-time, not a duty ceiling |
| Timezone | `America/New_York` default, overridable via `/usr/local/etc/sensors/time_zone` |

---

## First live readings — 2026-08-31

`garden-edge read` on the device, for tomorrow's comparison.

| | |
|---|---|
| Air temperature | 25.25 °C |
| Air humidity | 58.1 % |
| PCB temperature | 35.25 °C |
| Pump current | 0.5 mA (idle) |
| **Tank, factory value** | **6.8 cm**, `gy_wl` running, written 3 min earlier |

Five of the six base capabilities. Water temperature waits on the DS18B20 (Phase 5, and
on root); EC and pH are deferred hardware.

**Direction confirmed 2026-08-31.** Water was added and the reading fell from 67.34 mm
to 56.0 mm. Only a distance does that; a depth would have risen. So the factory value is
the distance from the sensor down to the water, matching what `garden-core` means by
`water_level_mm` — small when full, large when empty.

**Tank geometry, from the factory band.** `TankGeometry::STUDIO_2` now carries 30 mm full
and 250 mm empty, the factory clamp read as a distance. It replaces placeholders of 60
and 330 mm, where 330 was unreachable — an empty tank computed as 30% full, roughly 4.5
litres that were not there, and a dry-tank alert that would fire late or never.

The span checks out: 15.14 L over 220 mm needs a 688 cm² cross-section, 0.74 sq ft,
about half the 1.4 sq ft footprint. The alternative, had a full gallon been added to
produce that 11.34 mm drop, would need 3.6 sq ft — larger than the appliance.

Still inferred rather than measured. A jug and four readings settles it properly:

```sh
garden-cli tank calibrate --capacity 15.14 <mm>:<litres> <mm>:<litres> ...
```

---

## Confirmed against the hardware — 2026-08-31

Driven from the Gardyn app rather than by waiting for the clock, sampled at 1 Hz.

| Set in the app | `light_duty` | `light_source` |
|---|---|---|
| 0 % | `0.0000` | `pigpio-fifo` |
| 50 % | `0.5000` | `pigpio-fifo` |
| 100 % | `1.0000` | `pigpio-fifo` |

**The percent-to-duty mapping is exact and linear**, as `LED.py` says: `percent × 10 000`
of a 1 000 000 range at 20 kHz.

**The pump is digital.** Turned on manually for 13 seconds, it read `1.0000` under
`pigpio-level` — `gdc 24` answered `PI_NOT_PWM_GPIO` throughout, so the factory never
modulates it. First observation of the pump running; until now this was inference from
`Pump.py` alone.

**Transitions are ramps, not steps.** Three caught mid-flight — `0.0611` descending from
100 %, `0.2400` climbing to 50 %, `0.6135` climbing to 100 % — spanning roughly 1–2 s,
consistent with `ramp()`'s 1.5 s and with `hw_gm20` setting `withSunsetSunrise`.

**The gamma curve is confirmed too** — see the overnight capture below. An earlier note
here said 1 Hz could not resolve the shape and that roughly 20 Hz would be needed. That
was wrong: it can, given a hard lower bound on how long the ramp takes.

### Still unconfirmed by observation

The schedule *times*. Every reading above was forced from the app; that the firmware
actually steps at 07:00, 08:00, 21:00 and 22:00, and pumps at 07:00, 12:30, 17:30 and
22:55, still rests on reading `light_schedule.json` and `pump_schedule.json`. One
overnight `watch-pwm` closes it.

---

## Confirmed by observation — overnight 2026-08-31 → 09-01

`watch-pwm` at 1 Hz, 01:28–12:54 UTC, **40 121 samples, zero unavailable**. The device is
on `America/New_York`, so the CSV's UTC is four hours ahead of the schedule's local time.

| Scheduled (local) | Observed (UTC) | |
|---|---|---|
| 22:00 light off | `02:00:01` 0.5000 → 0.0000 | ✅ |
| 22:55–23:00 pump | `02:55:01`–`03:00:02` 1.0000, `pigpio-level` | ✅ |
| 07:00 light on (50 %) | `11:00:01` 0.0000 → 0.5000 | ✅ |
| 07:00–07:05 pump | `11:00:00`–`11:05:01` 1.0000, `pigpio-level` | ✅ |
| 08:00 boost (100 %) | `12:00:01` 0.5000 → 1.0000 | ✅ |

Every transition within one to three seconds of its scheduled time. **The schedule files
are what the firmware actually runs.** The 12:30 and 17:30 pump cycles fall outside this
window; the other two match, and the mechanism is the same for all four.

The pump read `pigpio-level` throughout both runs and `pigpio-fifo` never once — `gdc 24`
answers `PI_NOT_PWM_GPIO` even while the pump is running, which is `Pump.py`'s plain
digital write, observed rather than inferred.

### The ramp is the gamma curve, and it takes ~1.68 s

The 07:00 ramp put **two** samples inside one transition, 1.0265 s apart, at 0.0597 and
0.4594 of a 0 → 0.5 ramp. Inverting each candidate curve for the elapsed fraction:

| | implied ramp duration |
|---|---|
| gamma, `0.3α / (1.3 − α)` | **1.683 s** |
| linear | 1.284 s |

`LED.ramp()` runs 101 iterations of `time.sleep(1.5/100)`, so the ramp **cannot** be
shorter than 1.515 s whatever else is true. Linear requires 1.284 s and is therefore
impossible. The 08:00 ramp rules it out independently: 0.6000 partway through 0.5 → 1.0
puts linear at 80 % remaining, which would need a 1.283 s ramp again.

So the gamma formula is confirmed, and the real duration is about **1.68 s** rather than
the nominal 1.5 — the extra ~0.17 s is 101 pigpio socket round-trips on an ARMv6 core.
Phase 6 should reproduce the shape, and need not reproduce the overhead.
