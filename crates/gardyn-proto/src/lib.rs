//! The contract between the edge agent and the brain.
//!
//! Kept in its own crate with no I/O dependencies so both sides compile against the
//! same definitions. The edge agent cross-compiles to an ARM Pi and the brain runs on
//! x86 Linux; a shared crate is the only thing that stops the two drifting apart in a
//! way that only shows up as a silent parse failure at three in the morning.
//!
//! Transport is plain HTTPS with a bearer token today. MQTT topics are declared here
//! too, unused for now, because the topic strings belong with the payloads that travel
//! on them rather than being scattered across two binaries later.

pub mod recon;

use gardyn_core::{GardenId, SensorSnapshot};
use serde::{Deserialize, Serialize};

/// Bumped when a payload changes shape incompatibly.
///
/// The agent sends it on every request so the brain can tell "an old Pi that has not
/// been updated" from "a corrupt request", which are very different problems.
pub const PROTOCOL_VERSION: u32 = 1;

/// Header carrying [`PROTOCOL_VERSION`].
pub const VERSION_HEADER: &str = "x-gardyn-protocol";

/// Sensor readings, as posted to `POST /api/gardens/{id}/telemetry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryReport {
    pub protocol: u32,
    /// Agent build, surfaced on the fleet page so a stale Pi is visible.
    pub agent_version: String,
    pub sensors: SensorSnapshot,
    /// Raw pump draw for this sample, before smoothing. The brain keeps the running
    /// mean, because the baseline it compares against outlives any single agent run.
    pub pump_current_ma: Option<f32>,
}

impl TelemetryReport {
    pub fn new(sensors: SensorSnapshot, agent_version: impl Into<String>) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            agent_version: agent_version.into(),
            pump_current_ma: sensors.pump_current_ma,
            sensors,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryAccepted {
    /// Capabilities the brain inferred from the readings it just received.
    ///
    /// Echoed back so the agent can log what the brain thinks it has, which is the
    /// fastest way to spot a probe that is wired up but reading nothing.
    pub capabilities: Vec<String>,

    /// The schedule the agent should be running, if the brain has one for it.
    ///
    /// Carried on the telemetry response rather than pushed down a separate channel.
    /// The agent already makes this call every sample, so the schedule arrives without
    /// a new endpoint, a new connection, or anything for a firewall to allow inbound —
    /// and an agent that cannot reach the brain simply keeps the last one it was
    /// given, which is exactly the required behaviour.
    ///
    /// `None` means the brain has no opinion and the agent should keep running
    /// whatever it has. It never means "stop".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<gardyn_hal::Schedule>,
}

/// Registration, as posted to `POST /api/components/register`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub garden: Option<GardenId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat_seconds: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub id: String,
}

/// Liveness, as posted to `POST /api/components/{id}/heartbeat`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    /// `"ok"`, or a short reason the component considers itself degraded.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl HeartbeatRequest {
    pub fn ok(version: impl Into<String>) -> Self {
        Self {
            status: "ok".into(),
            version: Some(version.into()),
            detail: None,
        }
    }

    pub fn degraded(version: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            status: "degraded".into(),
            version: Some(version.into()),
            detail: Some(reason.into()),
        }
    }
}

/// Headers a frame upload carries. The body is the raw image.
pub mod frame_headers {
    pub const CAPTURED_AT: &str = "x-captured-at";
    pub const WIDTH: &str = "x-width";
    pub const HEIGHT: &str = "x-height";
    pub const LIGHT_DUTY_MILLI: &str = "x-light-duty-milli";
    /// `1`/`true`/`yes` when the lights were pinned to the reference level for the
    /// capture, which is what makes colour comparable between frames.
    pub const PHOTO_MODE: &str = "x-photo-mode";
}

/// MQTT topics, for when the transport moves off HTTP.
///
/// Declared now so the strings live beside the payloads rather than being invented
/// twice later. Nothing publishes on them yet.
pub mod topics {
    pub const TELEMETRY: &str = "gardyn/{garden}/telemetry";
    pub const FRAME: &str = "gardyn/{garden}/frame";
    pub const HEARTBEAT: &str = "gardyn/{garden}/heartbeat";
    pub const LIGHT_COMMAND: &str = "gardyn/{garden}/light/command";
    pub const PUMP_COMMAND: &str = "gardyn/{garden}/pump/command";
    /// Schedule updates pushed down to the agent. The brain never issues per-cycle
    /// commands — see the control-loop rule in DESIGN.md.
    pub const SCHEDULE: &str = "gardyn/{garden}/schedule";

    pub fn for_garden(template: &str, garden: &str) -> String {
        template.replace("{garden}", garden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gardyn_core::Timestamp;

    fn snapshot() -> SensorSnapshot {
        let mut s = SensorSnapshot::empty(Timestamp::from_second(1_700_000_000).unwrap());
        s.air_temp_c = Some(21.5);
        s.water_level_mm = Some(142.0);
        s.pump_current_ma = Some(412.0);
        s
    }

    #[test]
    fn a_telemetry_report_round_trips() {
        let report = TelemetryReport::new(snapshot(), "0.1.0");
        let json = serde_json::to_string(&report).unwrap();
        let back: TelemetryReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
        assert_eq!(back.protocol, PROTOCOL_VERSION);
    }

    #[test]
    fn absent_probes_survive_the_round_trip_as_absent() {
        // The distinction between "0.0" and "not fitted" is the whole capability
        // model; a serialisation that collapsed them would silently enable rules.
        let report = TelemetryReport::new(snapshot(), "0.1.0");
        let back: TelemetryReport =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert!(back.sensors.ec_ms_cm.is_none());
        assert!(back.sensors.ph.is_none());
        assert_eq!(back.sensors.air_temp_c, Some(21.5));
    }

    #[test]
    fn optional_registration_fields_are_omitted_rather_than_null() {
        let request = RegisterRequest {
            name: "kitchen-edge".into(),
            kind: "edge-agent".into(),
            garden: None,
            endpoint: None,
            heartbeat_seconds: Some(60),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("null"), "{json}");
        assert!(json.contains("heartbeat_seconds"));
    }

    #[test]
    fn a_degraded_heartbeat_carries_its_reason() {
        let beat = HeartbeatRequest::degraded("0.1.0", "AM2320 read timeout");
        assert_eq!(beat.status, "degraded");
        assert_eq!(beat.detail.as_deref(), Some("AM2320 read timeout"));
    }

    #[test]
    fn topics_interpolate_the_garden() {
        assert_eq!(
            topics::for_garden(topics::TELEMETRY, "abc"),
            "gardyn/abc/telemetry"
        );
    }
}
