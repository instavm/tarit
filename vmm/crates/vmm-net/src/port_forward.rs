//! Port forwarding — expose a TCP port on the VM to the outside world.
//!
//! PaaS use case: a VM hosts a web server on port 8080; the host forwards
//! port 8080 (or any host port) to the VM's IP:8080 via nftables DNAT.
//!
//! Two modes:
//!   - **DNAT** (default): rewrite the destination of incoming host packets
//!     to the VM's IP:port. The VM sees the original source IP.
//!   - **SNAT + DNAT**: also rewrite the source so the VM's reply traffic
//!     goes back through the host (needed if the VM's default route doesn't
//!     point at the host).
//!
//! Both use nftables rules in the host's `nat` table. The VM must be on a
//! TAP in a netns with a known IP (assigned by the orchestrator).

use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PortForwardError {
    #[error("invalid guest IP {guest_ip:?}: must be a valid IPv4 or IPv6 address")]
    InvalidGuestIp { guest_ip: String },
    #[error("invalid port-forward protocol {proto:?}: expected \"tcp\" or \"udp\"")]
    InvalidProto { proto: String },
}

/// A single port-forwarding rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortForward {
    /// Host port to listen on.
    pub host_port: u16,
    /// Guest IP to forward to (e.g. "172.16.0.2").
    pub guest_ip: String,
    /// Guest port to forward to.
    pub guest_port: u16,
    /// Protocol (tcp or udp).
    pub proto: String,
}

/// Compile a port-forward rule into nftables statements.
///
/// Generates:
///   `add rule inet vmm vmm_prerouting tcp dport <host_port> dnat to <guest_ip>:<guest_port>`
///   `add rule inet vmm vmm_postrouting ip saddr <guest_ip> tcp sport <guest_port> masquerade`
///
/// The `vmm_prerouting` chain hooks `prerouting` (DNAT); the `vmm_postrouting`
/// chain hooks `postrouting` (SNAT/masquerade for reply traffic).
pub fn compile_port_forward(pf: &PortForward) -> Vec<String> {
    try_compile_port_forward(pf).expect("valid port-forward rule")
}

/// Checked variant of [`compile_port_forward`] that rejects invalid inputs
/// before formatting nft command strings.
pub fn try_compile_port_forward(pf: &PortForward) -> Result<Vec<String>, PortForwardError> {
    validate_port_forward(pf)?;
    let mut rules = Vec::new();

    // DNAT: incoming traffic to host_port → guest_ip:guest_port.
    rules.push(format!(
        "add rule inet vmm vmm_prerouting {} dport {} dnat to {}:{}",
        pf.proto, pf.host_port, pf.guest_ip, pf.guest_port
    ));

    // SNAT/masquerade: reply traffic from the guest → masquerade as the host
    // so the external client sees the host's IP as the source.
    rules.push(format!(
        "add rule inet vmm vmm_postrouting ip saddr {} {} sport {} masquerade",
        pf.guest_ip, pf.proto, pf.guest_port
    ));

    Ok(rules)
}

/// Compile a full nftables table for port forwarding.
pub fn compile_port_forward_table(forwards: &[PortForward]) -> Vec<String> {
    try_compile_port_forward_table(forwards).expect("valid port-forward table")
}

/// Checked variant of [`compile_port_forward_table`] that rejects invalid
/// inputs before formatting nft command strings.
pub fn try_compile_port_forward_table(
    forwards: &[PortForward],
) -> Result<Vec<String>, PortForwardError> {
    let mut out = Vec::new();
    out.push("add table inet vmm".into());
    out.push(
        "add chain inet vmm vmm_prerouting { type nat hook prerouting priority -100; }".into(),
    );
    out.push(
        "add chain inet vmm vmm_postrouting { type nat hook postrouting priority 100; }".into(),
    );
    for pf in forwards {
        out.extend(try_compile_port_forward(pf)?);
    }
    Ok(out)
}

pub fn validate_port_forward(pf: &PortForward) -> Result<(), PortForwardError> {
    pf.guest_ip
        .parse::<IpAddr>()
        .map_err(|_| PortForwardError::InvalidGuestIp {
            guest_ip: pf.guest_ip.clone(),
        })?;
    validate_proto(&pf.proto)?;
    Ok(())
}

fn validate_proto(proto: &str) -> Result<(), PortForwardError> {
    match proto {
        "tcp" | "udp" => Ok(()),
        _ => Err(PortForwardError::InvalidProto {
            proto: proto.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_tcp_forward() {
        let pf = PortForward {
            host_port: 8080,
            guest_ip: "172.16.0.2".into(),
            guest_port: 80,
            proto: "tcp".into(),
        };
        let rules = compile_port_forward(&pf);
        assert_eq!(rules.len(), 2);
        assert!(rules[0].contains("tcp dport 8080 dnat to 172.16.0.2:80"));
        assert!(rules[1].contains("ip saddr 172.16.0.2 tcp sport 80 masquerade"));
    }

    #[test]
    fn compile_full_table() {
        let forwards = vec![PortForward {
            host_port: 443,
            guest_ip: "10.0.0.5".into(),
            guest_port: 8443,
            proto: "tcp".into(),
        }];
        let table = compile_port_forward_table(&forwards);
        assert!(table[0].contains("add table inet vmm"));
        assert!(table[1].contains("hook prerouting"));
        assert!(table[2].contains("hook postrouting"));
        assert!(table[3].contains("dport 443 dnat to 10.0.0.5:8443"));
    }

    #[test]
    fn forward_serializes_round_trip() {
        let pf = PortForward {
            host_port: 3000,
            guest_ip: "192.168.1.100".into(),
            guest_port: 3000,
            proto: "tcp".into(),
        };
        let s = serde_json::to_string(&pf).unwrap();
        let back: PortForward = serde_json::from_str(&s).unwrap();
        assert_eq!(back, pf);
    }

    #[test]
    fn checked_forward_rejects_injected_guest_ip() {
        let pf = PortForward {
            host_port: 8080,
            guest_ip: "172.16.0.2; flush ruleset".into(),
            guest_port: 80,
            proto: "tcp".into(),
        };
        let err = try_compile_port_forward(&pf).unwrap_err();
        assert!(matches!(err, PortForwardError::InvalidGuestIp { .. }));
    }

    #[test]
    fn checked_forward_rejects_injected_proto() {
        let pf = PortForward {
            host_port: 8080,
            guest_ip: "172.16.0.2".into(),
            guest_port: 80,
            proto: "tcp accept; flush ruleset".into(),
        };
        let err = try_compile_port_forward(&pf).unwrap_err();
        assert!(matches!(err, PortForwardError::InvalidProto { .. }));
    }
}
