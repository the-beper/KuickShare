//! What the Wi-Fi upgrade needs from this computer, and whether it has it (the data behind the
//! "System requirements" dialog and the gate on turning the upgrade on).
//!
//! One engine for diagnostics and dependency mapping, all in the core; the shell only renders.
//!
//! - `facts`: what the probes saw (plain data, no judgement);
//! - `rules`: facts and session evidence in, rows and capabilities out (pure);
//! - `policy`: what the capabilities allow when the upgrade is turned on;
//! - `engine`: runs the probes with time limits, caches, joins concurrent callers;
//! - `system`: the probe on the real system bus (read-only);
//! - `watch`: drops the cache when a service or a device changes;
//! - `model`: the JSON report (schema version 1).

pub mod engine;
pub mod facts;
pub mod model;
pub mod policy;
pub mod rules;
pub mod system;
pub mod watch;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub use engine::Budget;
pub use engine::{EngineConfig, RequirementsEngine, RequirementsProbe};
pub use facts::RuntimeEvidence;
pub use model::RequirementsReport;
pub use policy::{EnableVerdict, enable_verdict};
pub use system::SystemRequirementsProbe;

/// Version of the JSON report; a change that old readers cannot ignore raises it.
pub const SCHEMA_VERSION: u32 = 1;

/// The engine the daemon runs: the real probe.
pub type SystemEngine = RequirementsEngine<SystemRequirementsProbe>;
