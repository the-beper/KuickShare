//! What the report lets the Wi-Fi upgrade setting do when the user turns it on.

use super::model::{Capabilities, CapabilityStatus};

/// The answer to "may the upgrade be turned on?". Turning it off is always allowed and never
/// asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnableVerdict {
    /// Both directions are available.
    Allow,
    /// At least one direction works, with a limit or without the other direction.
    AllowLimited,
    /// Something could not be checked: the attempt is allowed, the shell warns.
    AllowUncertain,
    /// Both directions are proven blocked: nothing could use the setting.
    Reject,
}

/// Judges sending and receiving; joining a network does not depend on the upgrade setting.
pub fn enable_verdict(c: &Capabilities) -> EnableVerdict {
    use CapabilityStatus::{Available, Unavailable, Unknown};
    match (c.sending, c.receiving) {
        (Unavailable, Unavailable) => EnableVerdict::Reject,
        (Available, Available) => EnableVerdict::Allow,
        (Unknown, _) | (_, Unknown) => EnableVerdict::AllowUncertain,
        _ => EnableVerdict::AllowLimited,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CapabilityStatus::{Available, Degraded, Unavailable, Unknown};

    fn verdict(s: CapabilityStatus, r: CapabilityStatus) -> EnableVerdict {
        enable_verdict(&Capabilities {
            sending: s,
            receiving: r,
            join_network: Unavailable,
        })
    }

    #[test]
    fn the_table() {
        assert_eq!(verdict(Unavailable, Unavailable), EnableVerdict::Reject);
        assert_eq!(verdict(Available, Available), EnableVerdict::Allow);
        assert_eq!(verdict(Available, Unavailable), EnableVerdict::AllowLimited);
        assert_eq!(verdict(Unavailable, Degraded), EnableVerdict::AllowLimited);
        assert_eq!(verdict(Degraded, Degraded), EnableVerdict::AllowLimited);
        assert_eq!(verdict(Unknown, Unknown), EnableVerdict::AllowUncertain);
        assert_eq!(verdict(Unavailable, Unknown), EnableVerdict::AllowUncertain);
        assert_eq!(verdict(Available, Unknown), EnableVerdict::AllowUncertain);
    }
}
