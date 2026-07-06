//! Egress policy — host-enforced default-deny allowlist.
//!
//! "Default-deny egress; allow only an explicit per-VM/per-session allowlist
//! of CIDRs/ports." The guest cannot override this — all policy lives on the
//! host side of the tap.
//!
//! This module owns the policy *data model*; the *compiler* that turns an
//! `EgressPolicy` into nftables rules / an eBPF program is a separate module
//! landed in M8 / M14.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Proto {
    Tcp,
    Udp,
    Any,
}

/// A single allow rule. Default policy is deny-all.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EgressRule {
    /// CIDR the guest may reach (e.g. "10.0.0.0/8", "0.0.0.0/0" = anywhere).
    pub cidr: String,
    /// Destination port, or 0 for any.
    pub port: u16,
    pub proto: Proto,
}

/// Per-VM egress policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EgressPolicy {
    pub rules: Vec<EgressRule>,
}

impl EgressPolicy {
    pub fn allow_all() -> Self {
        Self {
            rules: vec![EgressRule {
                cidr: "0.0.0.0/0".into(),
                port: 0,
                proto: Proto::Any,
            }],
        }
    }

    pub fn deny_all() -> Self {
        Self { rules: vec![] }
    }

    pub fn add(&mut self, rule: EgressRule) {
        self.rules.push(rule);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_all_by_default() {
        assert!(EgressPolicy::deny_all().rules.is_empty());
    }

    #[test]
    fn allow_all_is_wildcard() {
        let p = EgressPolicy::allow_all();
        assert_eq!(p.rules.len(), 1);
        assert_eq!(p.rules[0].cidr, "0.0.0.0/0");
    }
}
