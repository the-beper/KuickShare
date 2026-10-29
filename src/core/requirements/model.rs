//! The report the core builds and the shell renders (JSON schema version 1, snake_case).
//!
//! The core never emits `Checking`, but parsers accept it. Every text field is bounded and
//! sanitized (see `rules::sanitize`); `details` and `device` never carry SSIDs, MACs, IPs,
//! passwords or the names of remote devices.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::SCHEMA_VERSION;

/// Longest `details` text, in characters.
pub const MAX_DETAILS: usize = 200;
/// Longest `verification` text, in characters.
pub const MAX_VERIFICATION: usize = 200;
/// Longest `device` text (an interface or adapter name), in characters.
pub const MAX_DEVICE: usize = 32;
/// Most rows in a report.
pub const MAX_ROWS: usize = 64;
/// Largest JSON document, in bytes.
pub const MAX_JSON_BYTES: usize = 64 * 1024;

/// `affects` value: the computer can send to a phone over Wi-Fi Direct.
pub const SENDING: &str = "sending";
/// `affects` value: the computer can receive from a phone over its own hotspot.
pub const RECEIVING: &str = "receiving";
/// `affects` value: the computer can join a network received from a phone.
pub const JOIN_NETWORK: &str = "join_network";

/// How one requirement stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ready,
    Warning,
    Blocked,
    Unknown,
    /// Never emitted by the core in version 1.
    Checking,
}

/// How much a requirement weighs in `derive`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// A blocked or unknown critical row decides the capability.
    Critical,
    /// A weaker requirement: it can only degrade.
    Warning,
    /// A security matter (the firewall, the sandbox): it can only degrade.
    Security,
    /// For the reader only; `derive` ignores it.
    Info,
}

/// What one capability amounts to, from the rows that feed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Available,
    /// Works with a limit (the shell says "Limited").
    Degraded,
    Unavailable,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub sending: CapabilityStatus,
    pub receiving: CapabilityStatus,
    pub join_network: CapabilityStatus,
}

/// How the daemon is installed; it decides which sandbox rows exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Packaging {
    Flatpak,
    Appimage,
    Native,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    /// Stable id such as `bluez.adapter`.
    pub id: String,
    pub status: Status,
    pub category: Category,
    /// Stable code such as `rfkill_blocked`; `ok` when the row is ready.
    pub reason_code: String,
    /// Fixed words, numbers and version strings only.
    pub details: String,
    /// Interface or adapter name only.
    pub device: Option<String>,
    /// How the row was checked, in fixed words.
    pub verification: String,
    /// Which of `SENDING`, `RECEIVING`, `JOIN_NETWORK` this row feeds.
    pub affects: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementsReport {
    pub schema_version: u32,
    /// Wall clock, milliseconds since the Unix epoch.
    pub generated_at_ms: u64,
    pub ttl_ms: u64,
    pub packaging: Packaging,
    pub core_version: String,
    pub capabilities: Capabilities,
    pub requirements: Vec<Requirement>,
}

impl RequirementsReport {
    /// Whether `other` says the same, whatever the time of the check.
    pub fn same_content(&self, other: &Self) -> bool {
        self.schema_version == other.schema_version
            && self.ttl_ms == other.ttl_ms
            && self.packaging == other.packaging
            && self.core_version == other.core_version
            && self.capabilities == other.capabilities
            && self.requirements == other.requirements
    }

    /// The JSON document of schema version 1. Fails only when it would pass `MAX_JSON_BYTES`.
    pub fn to_json(&self) -> Result<String> {
        debug_assert_eq!(self.schema_version, SCHEMA_VERSION);
        let json = serde_json::to_string(self)?;
        if json.len() > MAX_JSON_BYTES {
            bail!("the requirements report is {} bytes, too large", json.len());
        }
        Ok(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(requirements: Vec<Requirement>) -> RequirementsReport {
        RequirementsReport {
            schema_version: SCHEMA_VERSION,
            generated_at_ms: 1,
            ttl_ms: 30_000,
            packaging: Packaging::Native,
            core_version: "1.4.1".into(),
            capabilities: Capabilities {
                sending: CapabilityStatus::Available,
                receiving: CapabilityStatus::Unknown,
                join_network: CapabilityStatus::Degraded,
            },
            requirements,
        }
    }

    fn row() -> Requirement {
        Requirement {
            id: "nm.permissions".into(),
            status: Status::Warning,
            category: Category::Critical,
            reason_code: "auth_required".into(),
            details: "network-control=auth".into(),
            device: None,
            verification: "GetPermissions without interaction".into(),
            affects: vec![SENDING.into(), RECEIVING.into(), JOIN_NETWORK.into()],
        }
    }

    #[test]
    fn json_has_the_documented_shape() {
        let json = report(vec![row()]).to_json().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["ttl_ms"], 30_000);
        assert_eq!(v["packaging"], "native");
        assert_eq!(v["capabilities"]["join_network"], "degraded");
        assert_eq!(v["requirements"][0]["status"], "warning");
        assert_eq!(v["requirements"][0]["category"], "critical");
        assert!(v["requirements"][0]["device"].is_null());
        assert_eq!(v["requirements"][0]["affects"][2], "join_network");
        let back: RequirementsReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report(vec![row()]));
    }

    #[test]
    fn same_content_ignores_the_time() {
        let a = report(vec![row()]);
        let mut b = a.clone();
        b.generated_at_ms = 99;
        assert!(a.same_content(&b));
        b.requirements[0].status = Status::Blocked;
        assert!(!a.same_content(&b));
    }

    #[test]
    fn an_oversized_document_is_refused() {
        let mut big = row();
        big.details = "x".repeat(MAX_JSON_BYTES);
        assert!(report(vec![big]).to_json().is_err());
    }
}
