//! Capabilities: the single mechanism behind every optional feature.
//!
//! Deferred probes (EC, pH), each independently switchable vision stage, and actuator
//! ownership after firmware takeover are all modelled the same way. A rule declares
//! what it needs; the engine runs it only if the garden currently provides it.
//!
//! These are deliberately **runtime** state rather than Cargo features. A probe that
//! fails mid-season drops its capability and the calendar-estimate fallback resumes on
//! the next tick, with no redeploy.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    // --- Stock Studio 2 sensors -------------------------------------------------
    AirTemperature,
    AirHumidity,
    /// Ultrasonic tank distance, from which volume is derived.
    WaterLevel,
    /// INA219 on the pump. Doubles as a flow-restriction proxy.
    PumpCurrent,
    PcbTemperature,

    // --- Committed hardware addition --------------------------------------------
    /// DS18B20 in the reservoir. Drives dissolved-oxygen and root-rot reasoning.
    WaterTemperature,

    // --- Deferred hardware; software is ready, probes not yet purchased ---------
    Conductivity,
    PotentialHydrogen,

    // --- Vision stages, each independently switchable ---------------------------
    /// Phase A: HSV masking per slot ROI. No ML. Canopy area and colour statistics.
    CanopyMetrics,
    /// Phase B: ONNX segmentation. Per-plant masks, seedling counts, flower detection.
    PlantSegmentation,
    /// Phase C: local VLM. Qualitative diagnosis. Strictly advisory.
    VisualDiagnosis,

    // --- Actuators, acquired at firmware takeover -------------------------------
    LightControl,
    PumpControl,
}

impl Capability {
    /// What a stock, un-modified Studio 2 exposes read-only.
    pub const STOCK: &'static [Capability] = &[
        Capability::AirTemperature,
        Capability::AirHumidity,
        Capability::WaterLevel,
        Capability::PumpCurrent,
        Capability::PcbTemperature,
    ];

    /// Hardware we have committed to fitting.
    pub const COMMITTED: &'static [Capability] = &[Capability::WaterTemperature];

    /// Probes that are designed for but not yet purchased.
    pub const DEFERRED_HARDWARE: &'static [Capability] =
        &[Capability::Conductivity, Capability::PotentialHydrogen];

    pub const VISION: &'static [Capability] = &[
        Capability::CanopyMetrics,
        Capability::PlantSegmentation,
        Capability::VisualDiagnosis,
    ];

    pub const ACTUATORS: &'static [Capability] =
        &[Capability::LightControl, Capability::PumpControl];

    /// Human-readable label for rationale strings and the dashboard.
    pub fn label(self) -> &'static str {
        match self {
            Capability::AirTemperature => "air temperature",
            Capability::AirHumidity => "air humidity",
            Capability::WaterLevel => "water level",
            Capability::PumpCurrent => "pump current",
            Capability::PcbTemperature => "PCB temperature",
            Capability::WaterTemperature => "water temperature",
            Capability::Conductivity => "EC probe",
            Capability::PotentialHydrogen => "pH probe",
            Capability::CanopyMetrics => "canopy metrics",
            Capability::PlantSegmentation => "plant segmentation",
            Capability::VisualDiagnosis => "visual diagnosis",
            Capability::LightControl => "light control",
            Capability::PumpControl => "pump control",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// The set of capabilities the garden currently provides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet(BTreeSet<Capability>);

impl CapabilitySet {
    pub fn empty() -> Self {
        Self(BTreeSet::new())
    }

    /// Stock sensors only — what Phase 1 read-only telemetry gives us.
    pub fn stock() -> Self {
        Capability::STOCK.iter().copied().collect()
    }

    /// Stock plus the DS18B20 we are fitting. The near-term target configuration.
    pub fn committed() -> Self {
        let mut s = Self::stock();
        s.extend(Capability::COMMITTED.iter().copied());
        s
    }

    /// Everything, including deferred probes and all three vision stages. Used to
    /// verify that the high-precedence rules supersede their fallbacks correctly.
    pub fn fully_equipped() -> Self {
        Capability::STOCK
            .iter()
            .chain(Capability::COMMITTED)
            .chain(Capability::DEFERRED_HARDWARE)
            .chain(Capability::VISION)
            .chain(Capability::ACTUATORS)
            .copied()
            .collect()
    }

    #[must_use]
    pub fn with(mut self, c: Capability) -> Self {
        self.0.insert(c);
        self
    }

    #[must_use]
    pub fn without(mut self, c: Capability) -> Self {
        self.0.remove(&c);
        self
    }

    pub fn insert(&mut self, c: Capability) -> bool {
        self.0.insert(c)
    }

    pub fn remove(&mut self, c: Capability) -> bool {
        self.0.remove(&c)
    }

    pub fn contains(&self, c: Capability) -> bool {
        self.0.contains(&c)
    }

    pub fn contains_all(&self, required: &[Capability]) -> bool {
        required.iter().all(|c| self.0.contains(c))
    }

    /// Which of `required` are absent. Drives the "why is this rule inactive?" view.
    pub fn missing(&self, required: &[Capability]) -> Vec<Capability> {
        required
            .iter()
            .copied()
            .filter(|c| !self.0.contains(c))
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        self.0.iter().copied()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<I: IntoIterator<Item = Capability>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl Extend<Capability> for CapabilitySet {
    fn extend<I: IntoIterator<Item = Capability>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stock_excludes_deferred_hardware() {
        let stock = CapabilitySet::stock();
        assert!(!stock.contains(Capability::Conductivity));
        assert!(!stock.contains(Capability::PotentialHydrogen));
        assert!(stock.contains(Capability::WaterLevel));
    }

    #[test]
    fn committed_adds_only_water_temperature() {
        let stock = CapabilitySet::stock();
        let committed = CapabilitySet::committed();
        assert_eq!(committed.len(), stock.len() + 1);
        assert!(committed.contains(Capability::WaterTemperature));
    }

    #[test]
    fn missing_reports_the_gap() {
        let stock = CapabilitySet::stock();
        let gap = stock.missing(&[Capability::WaterLevel, Capability::Conductivity]);
        assert_eq!(gap, vec![Capability::Conductivity]);
    }

    #[test]
    fn capabilities_can_be_dropped_at_runtime() {
        // A probe failing mid-season must be expressible.
        let mut caps = CapabilitySet::fully_equipped();
        assert!(caps.remove(Capability::Conductivity));
        assert!(!caps.contains(Capability::Conductivity));
    }
}
