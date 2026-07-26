//! Sensor readings and derived signals.

use crate::capability::{Capability, CapabilitySet};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// One synchronised read of every sensor.
///
/// Every field is optional because capability presence is discovered at runtime: a
/// probe that is not fitted, or that has failed, simply reads `None` and the rules
/// depending on it stand down.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensorSnapshot {
    pub at: Timestamp,
    pub air_temp_c: Option<f32>,
    pub humidity_pct: Option<f32>,
    pub pcb_temp_c: Option<f32>,
    pub water_level_mm: Option<f32>,
    pub water_temp_c: Option<f32>,
    pub pump_current_ma: Option<f32>,
    pub ec_ms_cm: Option<f32>,
    pub ph: Option<f32>,
}

impl SensorSnapshot {
    pub fn empty(at: Timestamp) -> Self {
        Self {
            at,
            air_temp_c: None,
            humidity_pct: None,
            pcb_temp_c: None,
            water_level_mm: None,
            water_temp_c: None,
            pump_current_ma: None,
            ec_ms_cm: None,
            ph: None,
        }
    }

    /// Which sensing capabilities this reading actually demonstrates.
    ///
    /// Deriving capabilities from the data rather than from configuration means a
    /// failed probe degrades the system automatically, with no operator action.
    pub fn capabilities(&self) -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        let mut add = |present: bool, c: Capability| {
            if present {
                caps.insert(c);
            }
        };
        add(self.air_temp_c.is_some(), Capability::AirTemperature);
        add(self.humidity_pct.is_some(), Capability::AirHumidity);
        add(self.pcb_temp_c.is_some(), Capability::PcbTemperature);
        add(self.water_level_mm.is_some(), Capability::WaterLevel);
        add(self.water_temp_c.is_some(), Capability::WaterTemperature);
        add(self.pump_current_ma.is_some(), Capability::PumpCurrent);
        add(self.ec_ms_cm.is_some(), Capability::Conductivity);
        add(self.ph.is_some(), Capability::PotentialHydrogen);
        caps
    }
}

/// Tracks pump current against a clean-system baseline.
///
/// This is the cheapest diagnostic in the whole system: the INA219 is already fitted,
/// and a rising steady-state draw means the pump is working harder against a
/// restriction — root mass in the flow path or biofilm in the lines. It turns "prune
/// roots" and "clean" from calendar entries into measured triggers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PumpBaseline {
    /// Draw recorded immediately after a deep clean, with clear lines.
    pub nominal_ma: f32,
    /// Exponentially weighted mean of recent readings.
    pub current_ma_ewma: f32,
}

impl PumpBaseline {
    pub fn new(nominal_ma: f32) -> Self {
        Self {
            nominal_ma,
            current_ma_ewma: nominal_ma,
        }
    }

    /// Current draw as a multiple of the clean baseline. 1.0 is clean.
    pub fn restriction_ratio(&self) -> f32 {
        if self.nominal_ma <= 0.0 {
            return 1.0;
        }
        self.current_ma_ewma / self.nominal_ma
    }

    /// Fold a new reading into the running mean.
    pub fn observe(&mut self, reading_ma: f32, alpha: f32) {
        let a = alpha.clamp(0.0, 1.0);
        self.current_ma_ewma = self.current_ma_ewma * (1.0 - a) + reading_ma * a;
    }

    /// Re-baseline after a deep clean, when the system is known to be clear.
    pub fn rebaseline(&mut self) {
        self.nominal_ma = self.current_ma_ewma;
    }

    /// Restriction is worth a root check.
    pub const ADVISORY_RATIO: f32 = 1.15;
    /// Restriction is worth cleaning the system.
    pub const URGENT_RATIO: f32 = 1.35;
}

/// Exponentially weighted mean, used for consumption rate and sensor smoothing.
pub fn ewma(previous: f32, sample: f32, alpha: f32) -> f32 {
    let a = alpha.clamp(0.0, 1.0);
    previous * (1.0 - a) + sample * a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    #[test]
    fn capabilities_follow_the_data_not_the_config() {
        let mut s = SensorSnapshot::empty(t0());
        assert!(s.capabilities().is_empty());

        s.water_level_mm = Some(120.0);
        s.air_temp_c = Some(22.0);
        let caps = s.capabilities();
        assert!(caps.contains(Capability::WaterLevel));
        assert!(caps.contains(Capability::AirTemperature));
        assert!(!caps.contains(Capability::Conductivity));
    }

    #[test]
    fn a_failed_probe_drops_its_capability() {
        let mut s = SensorSnapshot::empty(t0());
        s.ec_ms_cm = Some(1.6);
        assert!(s.capabilities().contains(Capability::Conductivity));
        s.ec_ms_cm = None; // probe failure mid-season
        assert!(!s.capabilities().contains(Capability::Conductivity));
    }

    #[test]
    fn clean_pump_reads_unity_restriction() {
        let pump = PumpBaseline::new(400.0);
        assert_eq!(pump.restriction_ratio(), 1.0);
    }

    #[test]
    fn rising_draw_raises_the_restriction_ratio() {
        let mut pump = PumpBaseline::new(400.0);
        for _ in 0..200 {
            pump.observe(520.0, 0.1);
        }
        assert!((pump.restriction_ratio() - 1.3).abs() < 0.01);
        assert!(pump.restriction_ratio() > PumpBaseline::ADVISORY_RATIO);
    }

    #[test]
    fn rebaselining_after_a_clean_clears_the_alarm() {
        let mut pump = PumpBaseline::new(400.0);
        for _ in 0..200 {
            pump.observe(560.0, 0.1);
        }
        assert!(pump.restriction_ratio() > PumpBaseline::URGENT_RATIO);
        pump.rebaseline();
        assert!((pump.restriction_ratio() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn ewma_converges_on_the_sample() {
        let mut v = 0.0;
        for _ in 0..500 {
            v = ewma(v, 10.0, 0.2);
        }
        assert!((v - 10.0).abs() < 1e-3);
    }
}
