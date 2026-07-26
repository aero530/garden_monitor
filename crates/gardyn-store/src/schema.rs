//! Database schema.
//!
//! Applied on startup. Timestamps are RFC 3339 text: readable in a `sqlite3` shell,
//! round-trips through `jiff::Timestamp` without a conversion layer, and sortable as
//! text — but **only at fixed precision**. `jiff` prints the fewest fractional digits
//! it can, and mixed widths make `"…:20Z"` sort after `"…:20.5Z"`, which quietly drops
//! rows out of time windows. Everything is written with nine digits; see
//! `ts::PRECISION` and `NORMALISE_TIMESTAMPS` below. Ids are UUID text for the same
//! reason plus one more: they appear in URLs, and sequential ids under a sharing
//! model invite enumeration.

pub const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS users (
    id              TEXT PRIMARY KEY,
    email           TEXT NOT NULL UNIQUE,
    display_name    TEXT NOT NULL,
    password_digest TEXT NOT NULL,
    is_admin        INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL,
    disabled_at     TEXT
);

CREATE TABLE IF NOT EXISTS sessions (
    id           TEXT PRIMARY KEY,
    user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    digest       TEXT NOT NULL UNIQUE,
    created_at   TEXT NOT NULL,
    expires_at   TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    user_agent   TEXT
);
CREATE INDEX IF NOT EXISTS sessions_user ON sessions(user_id);

CREATE TABLE IF NOT EXISTS gardens (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    model      TEXT NOT NULL,
    timezone   TEXT NOT NULL DEFAULT 'UTC',
    created_at TEXT NOT NULL
);

-- The light and pump programme the Pi should be running, as a `gardyn_hal::Schedule`.
--
-- Handed to the agent on its next telemetry response, never pushed. The agent runs it
-- from its own clock and keeps running it when the brain is unreachable, which is the
-- load-bearing rule of the whole design: the brain is not in the control loop.
--
-- Absent means "no opinion", which the agent reads as "keep what you have". It never
-- means "stop", because a brain that has forgotten a garden must not be able to turn
-- its lights off.
CREATE TABLE IF NOT EXISTS garden_schedule (
    garden_id  TEXT PRIMARY KEY REFERENCES gardens(id) ON DELETE CASCADE,
    schedule   TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Sharing lives entirely in this table. A garden is shared exactly when it has more
-- than one row here; un-sharing is a delete. ON DELETE CASCADE means removing a user
-- or a garden cannot leave an orphaned grant behind.
CREATE TABLE IF NOT EXISTS memberships (
    garden_id  TEXT NOT NULL REFERENCES gardens(id) ON DELETE CASCADE,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role       TEXT NOT NULL,
    granted_by TEXT REFERENCES users(id) ON DELETE SET NULL,
    granted_at TEXT NOT NULL,
    PRIMARY KEY (garden_id, user_id)
);
CREATE INDEX IF NOT EXISTS memberships_user ON memberships(user_id);

CREATE TABLE IF NOT EXISTS invitations (
    id          TEXT PRIMARY KEY,
    garden_id   TEXT NOT NULL REFERENCES gardens(id) ON DELETE CASCADE,
    email       TEXT NOT NULL,
    role        TEXT NOT NULL,
    invited_by  TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    digest      TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    accepted_at TEXT,
    accepted_by TEXT REFERENCES users(id) ON DELETE SET NULL,
    revoked_at  TEXT
);
CREATE INDEX IF NOT EXISTS invitations_garden ON invitations(garden_id);
CREATE INDEX IF NOT EXISTS invitations_email ON invitations(email);

-- What is growing where, and what has been done to it.
--
-- Ids are per-garden integers rather than UUIDs because they already appear inside
-- task keys ("harvest:planting:8"), and those keys are only ever resolved after the
-- garden itself has been authorized — so there is nothing to enumerate.
--
-- Removed plantings stay for yield history, which is why the uniqueness constraint
-- below is partial rather than a plain UNIQUE.
CREATE TABLE IF NOT EXISTS plantings (
    garden_id       TEXT NOT NULL REFERENCES gardens(id) ON DELETE CASCADE,
    id              INTEGER NOT NULL,
    slot            INTEGER NOT NULL,
    variety_id      TEXT NOT NULL,
    planted_at      TEXT NOT NULL,
    germinated_at   TEXT,
    thinned_at      TEXT,
    last_root_check TEXT,
    last_prune      TEXT,
    last_harvest    TEXT,
    harvest_count   INTEGER NOT NULL DEFAULT 0,
    removed_at      TEXT,
    notes           TEXT,
    created_by      TEXT REFERENCES users(id) ON DELETE SET NULL,
    PRIMARY KEY (garden_id, id)
);

-- A slot holds at most one living plant. Enforced in the database rather than only in
-- application code: two people tending a shared garden can submit "plant slot 3" at
-- the same moment, and a check-then-insert in Rust would let both through.
CREATE UNIQUE INDEX IF NOT EXISTS plantings_one_live_per_slot
    ON plantings(garden_id, slot) WHERE removed_at IS NULL;

CREATE INDEX IF NOT EXISTS plantings_garden ON plantings(garden_id, removed_at);

-- Outstanding work. The rule engine is stateless and re-emits every tick, so this is
-- where completion, snoozing, and escalation actually live, keyed by the rules'
-- stable task key.
CREATE TABLE IF NOT EXISTS tasks (
    garden_id     TEXT NOT NULL REFERENCES gardens(id) ON DELETE CASCADE,
    task_key      TEXT NOT NULL,
    kind          TEXT NOT NULL,
    target        TEXT NOT NULL,
    severity      TEXT NOT NULL,
    rationale     TEXT NOT NULL,
    detail        TEXT,
    source_rule   TEXT NOT NULL,
    first_seen_at TEXT NOT NULL,
    due_at        TEXT NOT NULL,
    state         TEXT NOT NULL DEFAULT 'open',
    snoozed_until TEXT,
    completed_at  TEXT,
    completed_by  TEXT REFERENCES users(id) ON DELETE SET NULL,
    notified_at   TEXT,
    PRIMARY KEY (garden_id, task_key)
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(garden_id, state);

-- What the operator actually did, and what the sensors saw. Append-only.
CREATE TABLE IF NOT EXISTS events (
    id         TEXT PRIMARY KEY,
    garden_id  TEXT NOT NULL REFERENCES gardens(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,
    detail     TEXT,
    actor_id   TEXT REFERENCES users(id) ON DELETE SET NULL,
    occurred_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_garden_time ON events(garden_id, occurred_at DESC);

-- What was done to the reservoir, and when.
--
-- The rule engine is stateless: it re-derives "you are overdue for a refresh" from
-- `last_refresh` on every tick. Without this table those timestamps are always empty
-- on a real garden, so the maintenance rules fire immediately, stay fired, and ignore
-- the fact that you did the job — the same failure that planting events already fix
-- for the plants.
--
-- The payload is the JSON of `gardyn_core::TankEvent`, so adding a kind of event does
-- not need a migration.
CREATE TABLE IF NOT EXISTS tank_events (
    id          TEXT PRIMARY KEY,
    garden_id   TEXT NOT NULL REFERENCES gardens(id) ON DELETE CASCADE,
    payload     TEXT NOT NULL,
    actor_id    TEXT REFERENCES users(id) ON DELETE SET NULL,
    occurred_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tank_events_garden_time
    ON tank_events(garden_id, occurred_at);

-- One-tap notification links. Narrow by construction: one user, one task, one action,
-- one use. See gardyn_auth::action.
CREATE TABLE IF NOT EXISTS action_grants (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    garden_id  TEXT NOT NULL REFERENCES gardens(id) ON DELETE CASCADE,
    task_key   TEXT NOT NULL,
    action     TEXT NOT NULL,
    digest     TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    used_at    TEXT
);

-- Per-person notification settings.
--
-- Keyed on the user, not on (user, garden): quiet hours are a property of when you
-- sleep, not of which tower is asking.
CREATE TABLE IF NOT EXISTS notification_prefs (
    user_id         TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    -- ntfy topic. Null until the phone app is set up; treated as "no push".
    ntfy_topic      TEXT,
    email_enabled   INTEGER NOT NULL DEFAULT 0,
    quiet_from_hour INTEGER NOT NULL DEFAULT 21,
    quiet_to_hour   INTEGER NOT NULL DEFAULT 7,
    -- Offset from UTC in minutes. Quiet hours are meaningless without it, and the
    -- garden's timezone is the wrong one — the person might not live there.
    utc_offset_minutes INTEGER NOT NULL DEFAULT 0,
    -- Digest of the calendar feed secret. The secret itself is shown once.
    calendar_digest TEXT UNIQUE
);

-- What was sent, to whom, about what.
--
-- The dispatcher reads this to avoid re-announcing a task that has not changed. The
-- rules re-emit every tick, so without it the same root check would ping on every
-- evaluation — which is exactly how a notification system gets muted.
CREATE TABLE IF NOT EXISTS notifications (
    id         TEXT PRIMARY KEY,
    garden_id  TEXT NOT NULL REFERENCES gardens(id) ON DELETE CASCADE,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    task_key   TEXT NOT NULL,
    severity   TEXT NOT NULL,
    channels   TEXT NOT NULL,
    sent_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS notifications_lookup
    ON notifications(garden_id, user_id, task_key, sent_at DESC);

-- Sensor readings from the edge agent.
--
-- Every column is nullable because a probe that is not fitted reports nothing, and
-- the capability model turns on the difference between "absent" and "zero". Storing
-- 0.0 for a missing EC probe would silently enable measured dosing against a reading
-- that does not exist.
CREATE TABLE IF NOT EXISTS readings (
    garden_id       TEXT NOT NULL REFERENCES gardens(id) ON DELETE CASCADE,
    at              TEXT NOT NULL,
    air_temp_c      REAL,
    humidity_pct    REAL,
    pcb_temp_c      REAL,
    water_level_mm  REAL,
    water_temp_c    REAL,
    pump_current_ma REAL,
    ec_ms_cm        REAL,
    ph              REAL,
    agent_version   TEXT,
    PRIMARY KEY (garden_id, at)
);
CREATE INDEX IF NOT EXISTS readings_garden_time ON readings(garden_id, at DESC);

-- Where each slot is in the camera frame, as a `gardyn_vision::RoiMap` document.
--
-- One row per garden, and its presence is what switches vision on. That is not a
-- convenience: without knowing which pixels are slot 7 there is no way to measure
-- slot 7, so "no calibration" and "no canopy metrics" are the same fact rather than
-- two settings that can disagree. `gardyn-cli vision calibrate` writes it.
CREATE TABLE IF NOT EXISTS vision_config (
    garden_id  TEXT PRIMARY KEY REFERENCES gardens(id) ON DELETE CASCADE,
    roi_map    TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Per-slot measurements extracted from a frame.
--
-- Keyed by frame rather than only by time so a re-analysis after a threshold change
-- replaces its own rows instead of accumulating a second opinion beside the first.
-- The frame reference cascades: deleting a photograph deletes what was measured from
-- it, because a measurement whose evidence is gone cannot be checked.
CREATE TABLE IF NOT EXISTS slot_metrics (
    garden_id       TEXT NOT NULL REFERENCES gardens(id) ON DELETE CASCADE,
    frame_id        TEXT NOT NULL REFERENCES frames(id) ON DELETE CASCADE,
    slot            INTEGER NOT NULL,
    at              TEXT NOT NULL,
    canopy_area_cm2 REAL NOT NULL,
    green_fraction  REAL NOT NULL,
    yellowing_index REAL NOT NULL,
    growth_rate     REAL NOT NULL,
    plant_count     INTEGER,
    flowering       INTEGER,
    diagnosis       TEXT,
    PRIMARY KEY (frame_id, slot)
);
CREATE INDEX IF NOT EXISTS slot_metrics_garden_time
    ON slot_metrics(garden_id, slot, at DESC);

-- Algae coverage, which is a property of the tank rather than of a slot.
CREATE TABLE IF NOT EXISTS algae_readings (
    garden_id TEXT NOT NULL REFERENCES gardens(id) ON DELETE CASCADE,
    frame_id  TEXT NOT NULL REFERENCES frames(id) ON DELETE CASCADE,
    at        TEXT NOT NULL,
    coverage  REAL NOT NULL,
    PRIMARY KEY (frame_id)
);
CREATE INDEX IF NOT EXISTS algae_garden_time ON algae_readings(garden_id, at DESC);

-- Camera frames. The image bytes live on disk; this table is the index.
--
-- Blobs are kept out of SQLite deliberately: at one frame an hour a single garden
-- produces ~8,700 images a year, and putting them in the database would bloat every
-- backup and every `VACUUM INTO`.
CREATE TABLE IF NOT EXISTS frames (
    id           TEXT PRIMARY KEY,
    garden_id    TEXT NOT NULL REFERENCES gardens(id) ON DELETE CASCADE,
    captured_at  TEXT NOT NULL,
    width        INTEGER NOT NULL,
    height       INTEGER NOT NULL,
    content_type TEXT NOT NULL,
    byte_size    INTEGER NOT NULL,
    -- Light PWM duty at capture, in thousandths. Frames captured at different light
    -- levels are not photometrically comparable, so anything deriving colour from an
    -- image has to know this. Null means the agent did not report it.
    light_duty_milli INTEGER,
    -- Whether the capture was taken in photo mode, at the reference light level.
    -- Only comparable frames belong in a colour trend or a time-lapse.
    comparable   INTEGER NOT NULL DEFAULT 0,
    source       TEXT NOT NULL,
    created_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS frames_garden_time ON frames(garden_id, captured_at DESC);

-- Servers and applications: the edge agent, the broker, ntfy, the brain itself.
-- `garden_id` is null for infrastructure that is not tied to one device.
CREATE TABLE IF NOT EXISTS components (
    id           TEXT PRIMARY KEY,
    garden_id    TEXT REFERENCES gardens(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    kind         TEXT NOT NULL,
    version      TEXT,
    endpoint     TEXT,
    status       TEXT NOT NULL DEFAULT 'unknown',
    detail       TEXT,
    -- How long a component may stay silent before it is presumed down. Stored per
    -- component because an edge agent reporting every 30s and a nightly backup job
    -- have wildly different notions of "overdue".
    heartbeat_seconds INTEGER NOT NULL DEFAULT 120,
    last_seen_at TEXT,
    created_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS components_garden ON components(garden_id);
"#;

/// One-time normalisation of timestamps written before the precision was fixed.
///
/// `jiff` prints the fewest fractional digits it can, so early rows carry 0, 1, 3, 6
/// or 9 of them. SQLite compares those as text, which puts `"…:20Z"` *after*
/// `"…:20.5Z"` — so a row could sit inside a time window and be excluded from it. See
/// `ts::PRECISION`.
///
/// Idempotent: rows already 30 characters long are skipped, so this costs one indexed
/// scan per column on every start and does nothing after the first.
pub const NORMALISE_TIMESTAMPS: &str = r#"
UPDATE users SET created_at = substr(created_at, 1, 19) || '.' || substr(
    CASE WHEN instr(created_at, '.') > 0
         THEN substr(created_at, instr(created_at, '.') + 1, length(created_at) - instr(created_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE created_at IS NOT NULL AND length(created_at) <> 30 AND created_at LIKE '____-__-__T__:__:__%Z';

UPDATE users SET disabled_at = substr(disabled_at, 1, 19) || '.' || substr(
    CASE WHEN instr(disabled_at, '.') > 0
         THEN substr(disabled_at, instr(disabled_at, '.') + 1, length(disabled_at) - instr(disabled_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE disabled_at IS NOT NULL AND length(disabled_at) <> 30 AND disabled_at LIKE '____-__-__T__:__:__%Z';

UPDATE sessions SET created_at = substr(created_at, 1, 19) || '.' || substr(
    CASE WHEN instr(created_at, '.') > 0
         THEN substr(created_at, instr(created_at, '.') + 1, length(created_at) - instr(created_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE created_at IS NOT NULL AND length(created_at) <> 30 AND created_at LIKE '____-__-__T__:__:__%Z';

UPDATE sessions SET expires_at = substr(expires_at, 1, 19) || '.' || substr(
    CASE WHEN instr(expires_at, '.') > 0
         THEN substr(expires_at, instr(expires_at, '.') + 1, length(expires_at) - instr(expires_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE expires_at IS NOT NULL AND length(expires_at) <> 30 AND expires_at LIKE '____-__-__T__:__:__%Z';

UPDATE sessions SET last_seen_at = substr(last_seen_at, 1, 19) || '.' || substr(
    CASE WHEN instr(last_seen_at, '.') > 0
         THEN substr(last_seen_at, instr(last_seen_at, '.') + 1, length(last_seen_at) - instr(last_seen_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE last_seen_at IS NOT NULL AND length(last_seen_at) <> 30 AND last_seen_at LIKE '____-__-__T__:__:__%Z';

UPDATE gardens SET created_at = substr(created_at, 1, 19) || '.' || substr(
    CASE WHEN instr(created_at, '.') > 0
         THEN substr(created_at, instr(created_at, '.') + 1, length(created_at) - instr(created_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE created_at IS NOT NULL AND length(created_at) <> 30 AND created_at LIKE '____-__-__T__:__:__%Z';

UPDATE memberships SET granted_at = substr(granted_at, 1, 19) || '.' || substr(
    CASE WHEN instr(granted_at, '.') > 0
         THEN substr(granted_at, instr(granted_at, '.') + 1, length(granted_at) - instr(granted_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE granted_at IS NOT NULL AND length(granted_at) <> 30 AND granted_at LIKE '____-__-__T__:__:__%Z';

UPDATE invitations SET created_at = substr(created_at, 1, 19) || '.' || substr(
    CASE WHEN instr(created_at, '.') > 0
         THEN substr(created_at, instr(created_at, '.') + 1, length(created_at) - instr(created_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE created_at IS NOT NULL AND length(created_at) <> 30 AND created_at LIKE '____-__-__T__:__:__%Z';

UPDATE invitations SET expires_at = substr(expires_at, 1, 19) || '.' || substr(
    CASE WHEN instr(expires_at, '.') > 0
         THEN substr(expires_at, instr(expires_at, '.') + 1, length(expires_at) - instr(expires_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE expires_at IS NOT NULL AND length(expires_at) <> 30 AND expires_at LIKE '____-__-__T__:__:__%Z';

UPDATE invitations SET accepted_at = substr(accepted_at, 1, 19) || '.' || substr(
    CASE WHEN instr(accepted_at, '.') > 0
         THEN substr(accepted_at, instr(accepted_at, '.') + 1, length(accepted_at) - instr(accepted_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE accepted_at IS NOT NULL AND length(accepted_at) <> 30 AND accepted_at LIKE '____-__-__T__:__:__%Z';

UPDATE invitations SET revoked_at = substr(revoked_at, 1, 19) || '.' || substr(
    CASE WHEN instr(revoked_at, '.') > 0
         THEN substr(revoked_at, instr(revoked_at, '.') + 1, length(revoked_at) - instr(revoked_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE revoked_at IS NOT NULL AND length(revoked_at) <> 30 AND revoked_at LIKE '____-__-__T__:__:__%Z';

UPDATE plantings SET planted_at = substr(planted_at, 1, 19) || '.' || substr(
    CASE WHEN instr(planted_at, '.') > 0
         THEN substr(planted_at, instr(planted_at, '.') + 1, length(planted_at) - instr(planted_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE planted_at IS NOT NULL AND length(planted_at) <> 30 AND planted_at LIKE '____-__-__T__:__:__%Z';

UPDATE plantings SET germinated_at = substr(germinated_at, 1, 19) || '.' || substr(
    CASE WHEN instr(germinated_at, '.') > 0
         THEN substr(germinated_at, instr(germinated_at, '.') + 1, length(germinated_at) - instr(germinated_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE germinated_at IS NOT NULL AND length(germinated_at) <> 30 AND germinated_at LIKE '____-__-__T__:__:__%Z';

UPDATE plantings SET thinned_at = substr(thinned_at, 1, 19) || '.' || substr(
    CASE WHEN instr(thinned_at, '.') > 0
         THEN substr(thinned_at, instr(thinned_at, '.') + 1, length(thinned_at) - instr(thinned_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE thinned_at IS NOT NULL AND length(thinned_at) <> 30 AND thinned_at LIKE '____-__-__T__:__:__%Z';

UPDATE plantings SET removed_at = substr(removed_at, 1, 19) || '.' || substr(
    CASE WHEN instr(removed_at, '.') > 0
         THEN substr(removed_at, instr(removed_at, '.') + 1, length(removed_at) - instr(removed_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE removed_at IS NOT NULL AND length(removed_at) <> 30 AND removed_at LIKE '____-__-__T__:__:__%Z';

UPDATE tasks SET first_seen_at = substr(first_seen_at, 1, 19) || '.' || substr(
    CASE WHEN instr(first_seen_at, '.') > 0
         THEN substr(first_seen_at, instr(first_seen_at, '.') + 1, length(first_seen_at) - instr(first_seen_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE first_seen_at IS NOT NULL AND length(first_seen_at) <> 30 AND first_seen_at LIKE '____-__-__T__:__:__%Z';

UPDATE tasks SET due_at = substr(due_at, 1, 19) || '.' || substr(
    CASE WHEN instr(due_at, '.') > 0
         THEN substr(due_at, instr(due_at, '.') + 1, length(due_at) - instr(due_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE due_at IS NOT NULL AND length(due_at) <> 30 AND due_at LIKE '____-__-__T__:__:__%Z';

UPDATE tasks SET snoozed_until = substr(snoozed_until, 1, 19) || '.' || substr(
    CASE WHEN instr(snoozed_until, '.') > 0
         THEN substr(snoozed_until, instr(snoozed_until, '.') + 1, length(snoozed_until) - instr(snoozed_until, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE snoozed_until IS NOT NULL AND length(snoozed_until) <> 30 AND snoozed_until LIKE '____-__-__T__:__:__%Z';

UPDATE tasks SET completed_at = substr(completed_at, 1, 19) || '.' || substr(
    CASE WHEN instr(completed_at, '.') > 0
         THEN substr(completed_at, instr(completed_at, '.') + 1, length(completed_at) - instr(completed_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE completed_at IS NOT NULL AND length(completed_at) <> 30 AND completed_at LIKE '____-__-__T__:__:__%Z';

UPDATE tasks SET notified_at = substr(notified_at, 1, 19) || '.' || substr(
    CASE WHEN instr(notified_at, '.') > 0
         THEN substr(notified_at, instr(notified_at, '.') + 1, length(notified_at) - instr(notified_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE notified_at IS NOT NULL AND length(notified_at) <> 30 AND notified_at LIKE '____-__-__T__:__:__%Z';

UPDATE events SET occurred_at = substr(occurred_at, 1, 19) || '.' || substr(
    CASE WHEN instr(occurred_at, '.') > 0
         THEN substr(occurred_at, instr(occurred_at, '.') + 1, length(occurred_at) - instr(occurred_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE occurred_at IS NOT NULL AND length(occurred_at) <> 30 AND occurred_at LIKE '____-__-__T__:__:__%Z';

UPDATE tank_events SET occurred_at = substr(occurred_at, 1, 19) || '.' || substr(
    CASE WHEN instr(occurred_at, '.') > 0
         THEN substr(occurred_at, instr(occurred_at, '.') + 1, length(occurred_at) - instr(occurred_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE occurred_at IS NOT NULL AND length(occurred_at) <> 30 AND occurred_at LIKE '____-__-__T__:__:__%Z';

UPDATE action_grants SET created_at = substr(created_at, 1, 19) || '.' || substr(
    CASE WHEN instr(created_at, '.') > 0
         THEN substr(created_at, instr(created_at, '.') + 1, length(created_at) - instr(created_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE created_at IS NOT NULL AND length(created_at) <> 30 AND created_at LIKE '____-__-__T__:__:__%Z';

UPDATE action_grants SET expires_at = substr(expires_at, 1, 19) || '.' || substr(
    CASE WHEN instr(expires_at, '.') > 0
         THEN substr(expires_at, instr(expires_at, '.') + 1, length(expires_at) - instr(expires_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE expires_at IS NOT NULL AND length(expires_at) <> 30 AND expires_at LIKE '____-__-__T__:__:__%Z';

UPDATE action_grants SET used_at = substr(used_at, 1, 19) || '.' || substr(
    CASE WHEN instr(used_at, '.') > 0
         THEN substr(used_at, instr(used_at, '.') + 1, length(used_at) - instr(used_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE used_at IS NOT NULL AND length(used_at) <> 30 AND used_at LIKE '____-__-__T__:__:__%Z';

UPDATE notifications SET sent_at = substr(sent_at, 1, 19) || '.' || substr(
    CASE WHEN instr(sent_at, '.') > 0
         THEN substr(sent_at, instr(sent_at, '.') + 1, length(sent_at) - instr(sent_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE sent_at IS NOT NULL AND length(sent_at) <> 30 AND sent_at LIKE '____-__-__T__:__:__%Z';

UPDATE readings SET at = substr(at, 1, 19) || '.' || substr(
    CASE WHEN instr(at, '.') > 0
         THEN substr(at, instr(at, '.') + 1, length(at) - instr(at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE at IS NOT NULL AND length(at) <> 30 AND at LIKE '____-__-__T__:__:__%Z';

UPDATE vision_config SET updated_at = substr(updated_at, 1, 19) || '.' || substr(
    CASE WHEN instr(updated_at, '.') > 0
         THEN substr(updated_at, instr(updated_at, '.') + 1, length(updated_at) - instr(updated_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE updated_at IS NOT NULL AND length(updated_at) <> 30 AND updated_at LIKE '____-__-__T__:__:__%Z';

UPDATE slot_metrics SET at = substr(at, 1, 19) || '.' || substr(
    CASE WHEN instr(at, '.') > 0
         THEN substr(at, instr(at, '.') + 1, length(at) - instr(at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE at IS NOT NULL AND length(at) <> 30 AND at LIKE '____-__-__T__:__:__%Z';

UPDATE algae_readings SET at = substr(at, 1, 19) || '.' || substr(
    CASE WHEN instr(at, '.') > 0
         THEN substr(at, instr(at, '.') + 1, length(at) - instr(at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE at IS NOT NULL AND length(at) <> 30 AND at LIKE '____-__-__T__:__:__%Z';

UPDATE frames SET captured_at = substr(captured_at, 1, 19) || '.' || substr(
    CASE WHEN instr(captured_at, '.') > 0
         THEN substr(captured_at, instr(captured_at, '.') + 1, length(captured_at) - instr(captured_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE captured_at IS NOT NULL AND length(captured_at) <> 30 AND captured_at LIKE '____-__-__T__:__:__%Z';

UPDATE frames SET created_at = substr(created_at, 1, 19) || '.' || substr(
    CASE WHEN instr(created_at, '.') > 0
         THEN substr(created_at, instr(created_at, '.') + 1, length(created_at) - instr(created_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE created_at IS NOT NULL AND length(created_at) <> 30 AND created_at LIKE '____-__-__T__:__:__%Z';

UPDATE components SET last_seen_at = substr(last_seen_at, 1, 19) || '.' || substr(
    CASE WHEN instr(last_seen_at, '.') > 0
         THEN substr(last_seen_at, instr(last_seen_at, '.') + 1, length(last_seen_at) - instr(last_seen_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE last_seen_at IS NOT NULL AND length(last_seen_at) <> 30 AND last_seen_at LIKE '____-__-__T__:__:__%Z';

UPDATE components SET created_at = substr(created_at, 1, 19) || '.' || substr(
    CASE WHEN instr(created_at, '.') > 0
         THEN substr(created_at, instr(created_at, '.') + 1, length(created_at) - instr(created_at, '.') - 1)
         ELSE '' END || '000000000', 1, 9) || 'Z'
WHERE created_at IS NOT NULL AND length(created_at) <> 30 AND created_at LIKE '____-__-__T__:__:__%Z';
"#;
