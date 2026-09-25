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
    /// Draw recorded with clear lines, against which restriction is measured.
    ///
    /// `None` until one has been established. It used to be a hardcoded 400 mA whose
    /// own comment called it a placeholder, and nothing ever replaced it: `rebaseline`
    /// was called only by the simulator, and there was nowhere to persist a real one.
    /// So "23% above its clean baseline" meant "23% away from a number someone typed
    /// in", and a pump that simply drew 490 mA when spotless sat there forever.
    ///
    /// A reference that was never measured is not a reference, and reporting a
    /// percentage against one is worse than reporting nothing: it looks like a
    /// diagnostic.
    pub nominal_ma: Option<f32>,
    /// Mean draw across recent samples **taken while the pump was running**.
    ///
    /// `None` until such a sample exists, which is the common state rather than an
    /// edge case: the factory schedule runs the pump four times a day for five
    /// minutes, so it is drawing current for 1.4% of the day and roughly 99 samples
    /// in 100 catch it stopped.
    ///
    /// This was an `f32` holding whatever the newest reading said. A stopped pump
    /// draws nothing, so it read 0 mA nearly always, `restriction_ratio` came out at
    /// 0.0, and the dashboard reported a confident **−100% above clean baseline** —
    /// a pump using no power because it was switched off, presented as a measurement
    /// of how clear the lines are. An `Option` makes "nothing has been measured" a
    /// state the caller must handle rather than a number it can average.
    pub running_ma: Option<f32>,
}

impl PumpBaseline {
    pub fn new(nominal_ma: f32) -> Self {
        Self {
            nominal_ma: Some(nominal_ma),
            running_ma: None,
        }
    }

    /// A garden whose clean draw has never been measured.
    pub const fn unknown() -> Self {
        Self {
            nominal_ma: None,
            running_ma: None,
        }
    }

    /// Fewest running samples worth setting a baseline from.
    ///
    /// The pump draws for five minutes at a time, so at one sample a minute this is
    /// roughly a third of one cycle. Enough to average out the surge as it primes,
    /// few enough that a baseline exists within a day of the agent starting.
    pub const MIN_BASELINE_SAMPLES: i64 = 5;

    /// Draw below which the pump is considered stopped rather than unloaded.
    ///
    /// The INA219 reads a few mA of noise around zero with the pump off, and a
    /// restriction can only be measured against a pump that is actually pushing
    /// water. Well clear of the noise and well below any real running draw.
    pub const RUNNING_MA: f32 = 50.0;

    /// Current draw as a multiple of the clean baseline. 1.0 is clean.
    ///
    /// `None` when nothing has been measured with the pump running, because there is
    /// no honest answer then — and a rule that reads one anyway stands down to its
    /// calendar fallback, which is exactly what should happen.
    pub fn restriction_ratio(&self) -> Option<f32> {
        let nominal = self.nominal_ma?;
        if nominal <= 0.0 {
            return None;
        }
        Some(self.running_ma? / nominal)
    }

    /// Fold a new reading into the running mean.
    ///
    /// Samples below [`RUNNING_MA`] are discarded rather than averaged in: mixing the
    /// stopped intervals into the mean measures the duty cycle, not the restriction.
    pub fn observe(&mut self, reading_ma: f32, alpha: f32) {
        if reading_ma < Self::RUNNING_MA {
            return;
        }
        let a = alpha.clamp(0.0, 1.0);
        self.running_ma = Some(match self.running_ma {
            Some(mean) => mean * (1.0 - a) + reading_ma * a,
            // The first real sample is the mean; seeding from `nominal_ma` would drag
            // a genuinely restricted pump back towards clean for its first few reads.
            None => reading_ma,
        });
    }

    /// Re-baseline after a deep clean, when the system is known to be clear.
    ///
    /// A no-op until the pump has been measured running, because re-baselining to
    /// nothing would set `nominal_ma` from a pump that was off.
    pub fn rebaseline(&mut self) {
        if let Some(measured) = self.running_ma {
            self.nominal_ma = Some(measured);
        }
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
    fn a_pump_with_no_measured_reference_reports_nothing() {
        // Half the fix. `running_ma` being unknown was handled; `nominal_ma` being
        // unknown was not, because it was never unknown — it was a hardcoded 400 mA,
        // so every garden reported a confident percentage against a number that had
        // never been measured on any hardware.
        let mut pump = PumpBaseline::unknown();
        for _ in 0..50 {
            pump.observe(520.0, 0.1);
        }
        assert_eq!(pump.running_ma, Some(520.0), "the draw is known");
        assert_eq!(
            pump.restriction_ratio(),
            None,
            "but there is nothing to compare it against"
        );

        pump.nominal_ma = Some(400.0);
        assert!((pump.restriction_ratio().unwrap() - 1.3).abs() < 0.01);
    }

    #[test]
    fn a_pump_never_caught_running_reports_nothing_rather_than_unity() {
        // It used to report a clean 1.0, which is a claim about the lines made
        // without having measured them.
        let pump = PumpBaseline::new(400.0);
        assert_eq!(pump.restriction_ratio(), None);
    }

    #[test]
    fn a_stopped_pump_is_not_a_measurement_of_a_clear_line() {
        // The bug this whole shape exists to prevent. The factory schedule runs the
        // pump four times a day for five minutes, so nearly every sample catches it
        // at rest drawing nothing — and dividing that by the clean baseline gave a
        // ratio of 0.0, which the dashboard rendered as a confident
        // "-100% above clean baseline".
        let mut pump = PumpBaseline::new(400.0);
        for _ in 0..500 {
            pump.observe(0.0, 0.1);
        }
        assert_eq!(pump.running_ma, None);
        assert_eq!(pump.restriction_ratio(), None);

        // And one real sample among the silence is enough to measure.
        pump.observe(520.0, 0.1);
        assert_eq!(pump.running_ma, Some(520.0));
    }

    #[test]
    fn rising_draw_raises_the_restriction_ratio() {
        let mut pump = PumpBaseline::new(400.0);
        for _ in 0..200 {
            pump.observe(520.0, 0.1);
        }
        let ratio = pump.restriction_ratio().unwrap();
        assert!((ratio - 1.3).abs() < 0.01, "{ratio}");
        assert!(ratio > PumpBaseline::ADVISORY_RATIO);
    }

    #[test]
    fn idle_samples_between_cycles_do_not_dilute_the_measurement() {
        // A pump running at 520 mA for five minutes in every six hours is restricted
        // whether or not it is running right now. Averaging the stopped intervals in
        // would measure the duty cycle instead, and report roughly 1.4% of the truth.
        let mut restricted = PumpBaseline::new(400.0);
        for _ in 0..50 {
            for _ in 0..70 {
                restricted.observe(0.0, 0.1); // between cycles
            }
            for _ in 0..5 {
                restricted.observe(520.0, 0.1); // a cycle
            }
        }
        let ratio = restricted.restriction_ratio().unwrap();
        assert!((ratio - 1.3).abs() < 0.01, "{ratio}");
    }

    #[test]
    fn rebaselining_after_a_clean_clears_the_alarm() {
        let mut pump = PumpBaseline::new(400.0);
        for _ in 0..200 {
            pump.observe(560.0, 0.1);
        }
        assert!(pump.restriction_ratio().unwrap() > PumpBaseline::URGENT_RATIO);
        pump.rebaseline();
        assert!((pump.restriction_ratio().unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn rebaselining_an_unmeasured_pump_leaves_the_baseline_alone() {
        // A deep clean logged before the pump has ever been caught running must not
        // set the clean baseline from a pump that was switched off — that would
        // pin `nominal_ma` at nothing and make every later reading look catastrophic.
        let mut pump = PumpBaseline::new(400.0);
        pump.rebaseline();
        assert_eq!(pump.nominal_ma, Some(400.0));
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
