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
