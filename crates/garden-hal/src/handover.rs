//! Deciding which of two processes owns the pins.
//!
//! `garden-edge` drives the garden from its resident schedule. `garden-guard` takes
//! over when the agent stops beating. They must never drive the same pin at the same
//! time, and they live in separate processes precisely so that a panic in the
//! complicated one cannot take out the simple one — which rules out sharing any state
//! in memory.
//!
//! Two files, in the opposite directions:
//!
//! ```text
//!   edge  ──touches every tick──▶  edge.heartbeat  ──watched by──▶  guard
//!   edge  ◀──────watched by──────  guard.engaged   ◀──created by──  guard
//! ```
//!
//! **The handover is deliberately not atomic.** Both writers clamp through
//! [`Duty`](crate::Duty), so the worst a race can produce is one conflicting write of a
//! safe value, and the loser stands down on its next tick. Making it atomic would mean
//! a lock — and a lock is a way for the failsafe to be blocked by the very process it
//! exists to survive.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The file the failsafe creates when it seizes the pins.
#[derive(Debug, Clone)]
pub struct GuardMarker {
    path: PathBuf,
}

impl GuardMarker {
    pub const DEFAULT_PATH: &'static str = "/run/garden/guard.engaged";

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the failsafe currently owns the pins.
    pub fn engaged(&self) -> bool {
        self.path.exists()
    }

    /// Claim them. Called by the guard, never by the agent.
    pub fn engage(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, b"garden-guard\n")
    }

    /// Release them. Idempotent: the guard releases on stand-down and again on
    /// shutdown, and an already-absent marker is the desired state, not a failure.
    pub fn release(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

impl Default for GuardMarker {
    fn default() -> Self {
        Self::new(Self::DEFAULT_PATH)
    }
}

/// The file the agent touches to say it is alive.
#[derive(Debug, Clone)]
pub struct Heartbeat {
    path: PathBuf,
}

impl Heartbeat {
    pub const DEFAULT_PATH: &'static str = "/run/garden/edge.heartbeat";

    /// How long the agent may be silent before it is presumed dead.
    ///
    /// Generous on purpose. A brief stall during a frame upload must not start a
    /// handover; five minutes of silence is a dead process, five seconds is a busy one.
    pub const DEFAULT_GRACE: Duration = Duration::from_secs(300);

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record that the agent is alive.
    pub fn touch(&self, note: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, note.as_bytes())
    }

    /// How long since the last beat. `None` when the file has never been written.
    pub fn age(&self) -> Option<Duration> {
        let modified = std::fs::metadata(&self.path).ok()?.modified().ok()?;
        SystemTime::now().duration_since(modified).ok()
    }

    /// Whether the agent should be presumed dead.
    ///
    /// A missing file counts as infinitely stale: if the agent has never run, the
    /// garden still needs light and water.
    pub fn is_stale(&self, grace: Duration) -> bool {
        self.age().is_none_or(|age| age > grace)
    }
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self::new(Self::DEFAULT_PATH)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("garden-handover-{name}-{nanos}"))
    }

    #[test]
    fn a_marker_can_be_claimed_and_released() {
        let marker = GuardMarker::new(scratch("claim"));
        assert!(!marker.engaged());
        marker.engage().unwrap();
        assert!(marker.engaged());
        marker.release().unwrap();
        assert!(!marker.engaged());
    }

    #[test]
    fn releasing_twice_is_not_an_error() {
        let marker = GuardMarker::new(scratch("twice"));
        marker.engage().unwrap();
        marker.release().unwrap();
        marker.release().unwrap();
    }

    #[test]
    fn a_heartbeat_that_was_never_written_reads_as_dead() {
        // The case that matters most: if the agent has never run, the garden still
        // needs light and water, so "no file" must not read as "recently alive".
        let beat = Heartbeat::new(scratch("never"));
        assert_eq!(beat.age(), None);
        assert!(beat.is_stale(Heartbeat::DEFAULT_GRACE));
    }

    #[test]
    fn a_fresh_heartbeat_is_not_stale() {
        let beat = Heartbeat::new(scratch("fresh"));
        beat.touch("0.1.0").unwrap();
        assert!(beat.age().is_some());
        assert!(!beat.is_stale(Heartbeat::DEFAULT_GRACE));
        let _ = std::fs::remove_file(beat.path());
    }

    #[test]
    fn a_zero_grace_makes_any_heartbeat_stale() {
        // Sanity on the comparison direction: with no tolerance at all, even a beat
        // written a moment ago has aged past it.
        let beat = Heartbeat::new(scratch("zero"));
        beat.touch("0.1.0").unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(beat.is_stale(Duration::ZERO));
        let _ = std::fs::remove_file(beat.path());
    }

    #[test]
    fn touching_creates_the_run_directory() {
        // /run/garden does not exist on a fresh boot, and the agent must not need a
        // tmpfiles.d entry to be able to say it is alive.
        let dir = scratch("mkdir");
        let beat = Heartbeat::new(dir.join("nested").join("edge.heartbeat"));
        beat.touch("0.1.0").unwrap();
        assert!(beat.path().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
