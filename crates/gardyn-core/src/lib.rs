//! Domain model for the Gardyn Studio 2 management system.
//!
//! This crate is pure: no I/O, no async, no hardware. Everything here is a type or a
//! total function over types, which is what allows the rule engine, the simulator,
//! and the eventual brain to share one definition of what a garden *is*.
//!
//! The organising idea is [`Capability`](capability::Capability). Deferred probes,
//! each independently switchable vision stage, and actuator ownership after firmware
//! takeover are all the same mechanism: a rule declares what it needs, and the engine
//! runs it only when the garden currently provides it.

pub mod capability;
pub mod planting;
pub mod sensors;
pub mod slot;
pub mod state;
pub mod tank;
pub mod task;
pub mod time;
pub mod variety;
pub mod vision;

pub use capability::{Capability, CapabilitySet};
pub use planting::{Planting, PlantingId, Stage};
pub use sensors::{PumpBaseline, SensorSnapshot, ewma};
pub use slot::{Geometry, SlotId, SlotPosition};
pub use state::GardenState;
pub use tank::{DosingSpec, TankGeometry, TankState};
pub use task::{DueWindow, RuleId, Severity, Target, Task, TaskDetail, TaskKey, TaskKind};
pub use variety::{Category, CanopyClass, HarvestStyle, TargetRange, Variety, VarietyBook, VarietyId};
pub use vision::{AlgaeReading, LensCalibration, SlotMetrics};

pub use jiff::Timestamp;
