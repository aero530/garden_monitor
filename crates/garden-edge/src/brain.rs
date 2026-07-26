//! Talking to the brain, and surviving it being unreachable.
//!
//! The Pi sits on household wifi and the brain sits on a Proxmox VM that gets
//! rebooted for kernel updates. Losing samples every time either blinks would leave
//! holes in exactly the history the consumption and growth curves are fitted from, so
//! anything that fails to send is written to a spool directory and replayed later.

use garden_core::{GardenId, SensorSnapshot};
use garden_proto::{
    HeartbeatRequest, PROTOCOL_VERSION, RegisterRequest, RegisterResponse, TelemetryAccepted,
    TelemetryReport, VERSION_HEADER, frame_headers,
};
use jiff::Timestamp;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, thiserror::Error)]
pub enum BrainError {
    #[error("network: {0}")]
    Network(#[from] reqwest::Error),
    #[error("brain rejected the request: {status} {body}")]
    Rejected {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("spool: {0}")]
    Spool(String),
}

pub type Result<T> = std::result::Result<T, BrainError>;

#[derive(Debug, Clone)]
pub struct Client {
    base_url: String,
    token: String,
    garden: GardenId,
    http: reqwest::Client,
    spool: PathBuf,
}

impl Client {
    pub fn new(base_url: &str, token: &str, garden: GardenId, spool: PathBuf) -> Result<Self> {
        let http = reqwest::Client::builder()
            // Short, because a hung request must not stall the sampling loop; a
            // dropped sample goes to the spool and is retried.
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            garden,
            http,
            spool,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(BrainError::Rejected {
            status,
            body: body.chars().take(300).collect(),
        })
    }

    pub async fn register(&self, name: &str, heartbeat_seconds: i64) -> Result<String> {
        let response = self
            .http
            .post(self.url("/api/components/register"))
            .bearer_auth(&self.token)
            .header(VERSION_HEADER, PROTOCOL_VERSION.to_string())
            .json(&RegisterRequest {
                name: name.to_string(),
                kind: "edge-agent".into(),
                garden: Some(self.garden),
                endpoint: None,
                heartbeat_seconds: Some(heartbeat_seconds),
            })
            .send()
            .await?;
        let parsed: RegisterResponse = Self::check(response).await?.json().await?;
        Ok(parsed.id)
    }

    pub async fn heartbeat(&self, component: &str, beat: &HeartbeatRequest) -> Result<()> {
        let response = self
            .http
            .post(self.url(&format!("/api/components/{component}/heartbeat")))
            .bearer_auth(&self.token)
            .json(beat)
            .send()
            .await?;
        Self::check(response).await.map(|_| ())
    }

    /// Send a sample, spooling it to disk if the brain cannot be reached.
    pub async fn send_telemetry(&self, sensors: &SensorSnapshot) -> Result<TelemetryAccepted> {
        let report = TelemetryReport::new(sensors.clone(), AGENT_VERSION);
        match self.post_telemetry(&report).await {
            Ok(accepted) => Ok(accepted),
            Err(e) => {
                self.spool_telemetry(&report)?;
                Err(e)
            }
        }
    }

    async fn post_telemetry(&self, report: &TelemetryReport) -> Result<TelemetryAccepted> {
        let response = self
            .http
            .post(self.url(&format!("/api/gardens/{}/telemetry", self.garden)))
            .bearer_auth(&self.token)
            .header(VERSION_HEADER, PROTOCOL_VERSION.to_string())
            .json(report)
            .send()
            .await?;
        Ok(Self::check(response).await?.json().await?)
    }

    pub async fn upload_frame(
        &self,
        bytes: Vec<u8>,
        captured_at: Timestamp,
        width: u32,
        height: u32,
        light_duty_milli: Option<i64>,
        photo_mode: bool,
    ) -> Result<()> {
        let mut request = self
            .http
            .post(self.url(&format!("/api/gardens/{}/frames", self.garden)))
            .bearer_auth(&self.token)
            .header(frame_headers::CAPTURED_AT, captured_at.to_string())
            .header(frame_headers::WIDTH, width.to_string())
            .header(frame_headers::HEIGHT, height.to_string())
            .header(frame_headers::PHOTO_MODE, if photo_mode { "1" } else { "0" });

        if let Some(duty) = light_duty_milli {
            request = request.header(frame_headers::LIGHT_DUTY_MILLI, duty.to_string());
        }

        Self::check(request.body(bytes).send().await?).await.map(|_| ())
    }

    // --- Spool ------------------------------------------------------------------

    fn spool_telemetry(&self, report: &TelemetryReport) -> Result<()> {
        std::fs::create_dir_all(&self.spool)
            .map_err(|e| BrainError::Spool(format!("creating {}: {e}", self.spool.display())))?;

        // Named by the sample's own timestamp, so replay is chronological and a
        // duplicate sample overwrites rather than accumulating. The brain upserts on
        // (garden, at) too, so a double send is harmless either way.
        let name = report.sensors.at.as_second().to_string();
        let path = self.spool.join(format!("{name}.json"));
        let json = serde_json::to_vec(report)
            .map_err(|e| BrainError::Spool(format!("encoding: {e}")))?;
        std::fs::write(&path, json)
            .map_err(|e| BrainError::Spool(format!("writing {}: {e}", path.display())))
    }

    /// Replay spooled samples oldest first. Stops at the first failure so a still-down
    /// brain is not hammered with the whole backlog.
    pub async fn drain_spool(&self) -> Result<usize> {
        let mut files = match std::fs::read_dir(&self.spool) {
            Ok(entries) => entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .collect::<Vec<_>>(),
            // No spool directory means nothing was ever buffered.
            Err(_) => return Ok(0),
        };
        files.sort();

        let mut sent = 0;
        for path in files {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(report) = serde_json::from_slice::<TelemetryReport>(&bytes) else {
                // Unparseable spool entries are dropped rather than retried forever.
                let _ = std::fs::remove_file(&path);
                continue;
            };
            self.post_telemetry(&report).await?;
            let _ = std::fs::remove_file(&path);
            sent += 1;
        }
        Ok(sent)
    }

    pub fn spooled_count(&self) -> usize {
        std::fs::read_dir(&self.spool)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .count()
            })
            .unwrap_or(0)
    }

    pub fn spool_dir(&self) -> &Path {
        &self.spool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_spool() -> PathBuf {
        std::env::temp_dir().join(format!("garden-spool-test-{}", jiff::Timestamp::now().as_nanosecond()))
    }

    fn client(spool: PathBuf) -> Client {
        // Unroutable by design: every send must fail so the spool path is exercised.
        Client::new("http://127.0.0.1:1", "token", GardenId::new(), spool).unwrap()
    }

    fn snapshot(at_second: i64) -> SensorSnapshot {
        let mut s = SensorSnapshot::empty(Timestamp::from_second(at_second).unwrap());
        s.air_temp_c = Some(21.0);
        s
    }

    #[tokio::test]
    async fn a_failed_send_is_spooled_rather_than_lost() {
        let spool = temp_spool();
        let client = client(spool.clone());

        assert!(client.send_telemetry(&snapshot(1_700_000_000)).await.is_err());
        assert_eq!(client.spooled_count(), 1, "the sample should be on disk");

        let _ = std::fs::remove_dir_all(&spool);
    }

    #[tokio::test]
    async fn spooling_the_same_sample_twice_does_not_accumulate() {
        // A retry loop must not fill the SD card with copies of one reading.
        let spool = temp_spool();
        let client = client(spool.clone());

        for _ in 0..5 {
            let _ = client.send_telemetry(&snapshot(1_700_000_000)).await;
        }
        assert_eq!(client.spooled_count(), 1);

        let _ = std::fs::remove_dir_all(&spool);
    }

    #[tokio::test]
    async fn distinct_samples_each_get_spooled() {
        let spool = temp_spool();
        let client = client(spool.clone());

        for offset in 0..3 {
            let _ = client.send_telemetry(&snapshot(1_700_000_000 + offset * 60)).await;
        }
        assert_eq!(client.spooled_count(), 3);

        let _ = std::fs::remove_dir_all(&spool);
    }

    #[tokio::test]
    async fn draining_an_absent_spool_is_not_an_error() {
        let client = client(temp_spool());
        assert_eq!(client.drain_spool().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn draining_stops_at_the_first_failure_and_keeps_the_backlog() {
        // The brain is still down; the samples must survive for the next attempt.
        let spool = temp_spool();
        let client = client(spool.clone());
        for offset in 0..3 {
            let _ = client.send_telemetry(&snapshot(1_700_000_000 + offset * 60)).await;
        }

        assert!(client.drain_spool().await.is_err());
        assert_eq!(client.spooled_count(), 3, "nothing should have been dropped");

        let _ = std::fs::remove_dir_all(&spool);
    }

    #[tokio::test]
    async fn an_unparseable_spool_entry_is_discarded_not_retried_forever() {
        let spool = temp_spool();
        std::fs::create_dir_all(&spool).unwrap();
        std::fs::write(spool.join("garbage.json"), b"{ not json").unwrap();

        let client = client(spool.clone());
        // No valid entries, so the drain completes without contacting anything.
        assert_eq!(client.drain_spool().await.unwrap(), 0);
        assert_eq!(client.spooled_count(), 0, "the bad entry should be gone");

        let _ = std::fs::remove_dir_all(&spool);
    }

    #[test]
    fn a_trailing_slash_on_the_base_url_does_not_double_up() {
        let client = Client::new(
            "http://brain.local:8080/",
            "t",
            GardenId::new(),
            temp_spool(),
        )
        .unwrap();
        assert_eq!(client.url("/healthz"), "http://brain.local:8080/healthz");
    }
}
