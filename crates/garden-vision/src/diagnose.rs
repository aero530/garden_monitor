//! Stage C: a plain-language second opinion from a local vision model.
//!
//! **Strictly advisory.** Deterministic rules own everything that touches dosing,
//! water, or an actuator. A model that invents a nitrogen deficiency must not be able
//! to dose the tank, and the way that guarantee is enforced is structural: this module
//! produces a `String` and nothing else. There is no path from here to a `Task`.
//!
//! Self-hosted, like everything else. The backend is a local Ollama, not a hosted API,
//! and it sits behind a trait so swapping the model or the runtime does not touch the
//! pipeline.

use std::fmt;

/// What the model was asked about.
#[derive(Debug, Clone)]
pub struct DiagnosisRequest {
    /// PNG or JPEG bytes of the slot's region, already cropped.
    pub image: Vec<u8>,
    /// What is planted there, so the model is not guessing at the species too.
    pub variety: String,
    /// Days since germination.
    pub age_days: f64,
    /// What stage A already measured, so the model has the numbers rather than
    /// estimating them from the picture.
    pub measured: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DiagnosisError {
    #[error("diagnosis backend unreachable: {0}")]
    Unreachable(String),
    #[error("diagnosis backend returned {status}: {body}")]
    Rejected { status: u16, body: String },
    #[error("diagnosis backend is not built into this binary; enable the `diagnosis` feature")]
    NotCompiled,
}

/// A source of qualitative assessments.
///
/// Not async-trait: the only implementation is HTTP and the caller is already async, so
/// this returns a boxed future rather than pulling in a macro crate for one method.
pub trait DiagnosisBackend: Send + Sync {
    fn describe(
        &self,
        request: DiagnosisRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, DiagnosisError>> + Send + '_>,
    >;
}

/// The prompt. Kept here rather than inline so the wording is reviewable.
///
/// Three things it deliberately does: gives the model the measurements instead of
/// letting it estimate them, asks for absence of a problem to be stated plainly, and
/// caps the length. An unbounded model writes three paragraphs about photosynthesis.
pub fn prompt(request: &DiagnosisRequest) -> String {
    format!(
        "This is one plant in a hydroponic tower: {variety}, {age:.0} days since it \
sprouted. Measured from the image: {measured}.\n\n\
Describe what you can see about this plant's health in at most two sentences. \
Mention only what is visible — leaf colour, wilting, spotting, pests, physical \
damage. Do not recommend nutrient changes or watering; those are decided elsewhere \
from sensor data. If the plant looks healthy, say so in one short sentence.",
        variety = request.variety,
        age = request.age_days,
        measured = request.measured,
    )
}

/// Where the model lives.
#[derive(Debug, Clone)]
pub struct OllamaConfig {
    /// Base URL. On the standard deployment this is the container name on the shared
    /// Podman network, not a host address.
    pub base_url: String,
    /// Model tag, e.g. `qwen2.5vl:7b`.
    pub model: String,
    /// Give up after this. A vision model on CPU is slow, and the dispatcher cannot
    /// block a five-minute sweep behind it.
    pub timeout: std::time::Duration,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            base_url: "http://garden-ollama:11434".into(),
            model: "qwen2.5vl:7b".into(),
            timeout: std::time::Duration::from_secs(120),
        }
    }
}

impl fmt::Display for OllamaConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}", self.model, self.base_url)
    }
}

#[cfg(feature = "diagnosis")]
mod ollama {
    use super::*;
    use serde::Deserialize;

    pub struct OllamaBackend {
        config: OllamaConfig,
        client: reqwest::Client,
    }

    #[derive(Deserialize)]
    struct GenerateResponse {
        response: String,
    }

    impl OllamaBackend {
        pub fn new(config: OllamaConfig) -> Result<Self, DiagnosisError> {
            let client = reqwest::Client::builder()
                .timeout(config.timeout)
                .build()
                .map_err(|e| DiagnosisError::Unreachable(e.to_string()))?;
            Ok(Self { config, client })
        }
    }

    impl DiagnosisBackend for OllamaBackend {
        fn describe(
            &self,
            request: DiagnosisRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, DiagnosisError>> + Send + '_>,
        > {
            Box::pin(async move {
                let body = serde_json::json!({
                    "model": self.config.model,
                    "prompt": prompt(&request),
                    "images": [base64(&request.image)],
                    "stream": false,
                    "options": { "temperature": 0.2, "num_predict": 160 },
                });

                let response = self
                    .client
                    .post(format!("{}/api/generate", self.config.base_url))
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| DiagnosisError::Unreachable(e.to_string()))?;

                let status = response.status();
                if !status.is_success() {
                    let body = response.text().await.unwrap_or_default();
                    return Err(DiagnosisError::Rejected {
                        status: status.as_u16(),
                        body: body.chars().take(200).collect(),
                    });
                }

                let parsed: GenerateResponse = response
                    .json()
                    .await
                    .map_err(|e| DiagnosisError::Unreachable(e.to_string()))?;
                Ok(parsed.response.trim().to_string())
            })
        }
    }

    /// Ollama wants images as base64. Sixteen lines beats a dependency.
    fn base64(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(TABLE[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn base64_matches_the_rfc_test_vectors() {
            assert_eq!(base64(b""), "");
            assert_eq!(base64(b"f"), "Zg==");
            assert_eq!(base64(b"fo"), "Zm8=");
            assert_eq!(base64(b"foo"), "Zm9v");
            assert_eq!(base64(b"foob"), "Zm9vYg==");
            assert_eq!(base64(b"fooba"), "Zm9vYmE=");
            assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        }

        #[test]
        fn binary_bytes_survive_the_encoder() {
            // A JPEG is not ASCII; the high bit and the zero byte both have to work.
            assert_eq!(base64(&[0xFF, 0xD8, 0xFF]), "/9j/");
            assert_eq!(base64(&[0x00, 0x00, 0x00]), "AAAA");
        }
    }
}

#[cfg(feature = "diagnosis")]
pub use ollama::OllamaBackend;

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> DiagnosisRequest {
        DiagnosisRequest {
            image: vec![0xFF, 0xD8],
            variety: "Lacinato Kale".into(),
            age_days: 41.0,
            measured: "canopy 320 cm², 18% yellowing, growth has stalled".into(),
        }
    }

    #[test]
    fn the_prompt_carries_the_measurements_rather_than_asking_for_them() {
        let text = prompt(&request());
        assert!(text.contains("Lacinato Kale"));
        assert!(text.contains("41 days"));
        assert!(text.contains("320 cm²"));
    }

    #[test]
    fn the_prompt_forbids_the_model_from_prescribing() {
        // The structural guarantee is that this returns a String. This is the belt to
        // that braces: the model is told the dosing decision is not its to make.
        let text = prompt(&request());
        assert!(text.contains("Do not recommend nutrient changes"));
        assert!(text.contains("decided elsewhere"));
    }

    #[test]
    fn the_prompt_bounds_the_answer() {
        assert!(prompt(&request()).contains("at most two sentences"));
    }

    #[test]
    fn the_default_backend_is_a_container_on_the_private_network() {
        // Self-hosted is the standing constraint: no hosted API, and not exposed.
        let config = OllamaConfig::default();
        assert!(config.base_url.starts_with("http://garden-ollama"));
        assert!(!config.base_url.contains("api.") && !config.base_url.contains("https://"));
    }
}
