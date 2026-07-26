//! Database schema.
//!
//! Applied on startup. Timestamps are RFC 3339 text because that sorts correctly as a
//! string, survives a `sqlite3` shell session unmangled, and round-trips through
//! `jiff::Timestamp` without a conversion layer. Ids are UUID text for the same
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
