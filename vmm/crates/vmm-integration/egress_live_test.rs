//! Live egress policy update e2e test.
//!
//! PRD §8: egress allowlist must be updatable while the VM is running.
//! This test compiles an egress update, verifies the nftables commands
//! are correct, and simulates applying them.

use vmm_net::egress::{EgressPolicy, EgressRule, Proto};
use vmm_net::live_egress::{compile_egress_update, diff_policies, EgressUpdate};

#[test]
fn live_egress_update_compiles_nft_rules() {
    let update = EgressUpdate {
        vm_id: "vm-1".into(),
        policy: EgressPolicy {
            rules: vec![
                EgressRule {
                    cidr: "10.0.0.0/8".into(),
                    port: 443,
                    proto: Proto::Tcp,
                },
                EgressRule {
                    cidr: "8.8.8.8/32".into(),
                    port: 53,
                    proto: Proto::Udp,
                },
            ],
        },
        allow_existing: true,
    };

    let result = compile_egress_update(&update);

    // Must flush the chain first.
    assert!(result.nft_commands[0].contains("flush chain"));
    // Must always allow replies for permitted outbound connections.
    assert!(result.nft_commands[1].contains("ct state established"));
    // Must have the two allow rules.
    assert!(result.nft_commands[2].contains("10.0.0.0/8"));
    assert!(result.nft_commands[2].contains("tcp dport 443"));
    assert!(result.nft_commands[3].contains("8.8.8.8/32"));
    assert!(result.nft_commands[3].contains("udp dport 53"));
    assert_eq!(result.rules_applied, 2);
}

#[test]
fn live_egress_diff_detects_changes() {
    let old = EgressPolicy {
        rules: vec![EgressRule {
            cidr: "0.0.0.0/0".into(),
            port: 0,
            proto: Proto::Any,
        }],
    };
    let new = EgressPolicy {
        rules: vec![EgressRule {
            cidr: "10.0.0.0/8".into(),
            port: 443,
            proto: Proto::Tcp,
        }],
    };

    let diff = diff_policies(&old, &new);
    assert_eq!(diff.added.len(), 1);
    assert_eq!(diff.removed.len(), 1);
    assert_eq!(diff.added[0].cidr, "10.0.0.0/8");
    assert_eq!(diff.removed[0].cidr, "0.0.0.0/0");
}

#[test]
fn live_egress_deny_all_update() {
    let update = EgressUpdate {
        vm_id: "vm-2".into(),
        policy: EgressPolicy::deny_all(),
        allow_existing: false,
    };

    let result = compile_egress_update(&update);
    // Flush + stateful reply rule + no allow rules (deny-all).
    assert!(result.nft_commands[0].contains("flush chain"));
    assert!(result.nft_commands[1].contains("ct state established"));
    assert_eq!(result.nft_commands.len(), 2);
    assert_eq!(result.rules_applied, 0);
}

#[test]
fn live_egress_via_api_roundtrip() {
    // Simulate the API path: ApiRequest::UpdateEgress → compile → verify.
    let allowlist = [
        "10.0.0.0/8:443/tcp".to_string(),
        "0.0.0.0/0:80/tcp".to_string(),
    ];

    let rules: Vec<EgressRule> = allowlist
        .iter()
        .map(|s| {
            if let Some((cidr, rest)) = s.split_once(':') {
                if let Some((port, proto)) = rest.split_once('/') {
                    EgressRule {
                        cidr: cidr.into(),
                        port: port.parse().unwrap_or(0),
                        proto: match proto {
                            "tcp" => Proto::Tcp,
                            "udp" => Proto::Udp,
                            _ => Proto::Any,
                        },
                    }
                } else {
                    EgressRule {
                        cidr: cidr.into(),
                        port: rest.parse().unwrap_or(0),
                        proto: Proto::Tcp,
                    }
                }
            } else {
                EgressRule {
                    cidr: s.into(),
                    port: 0,
                    proto: Proto::Any,
                }
            }
        })
        .collect();

    let update = EgressUpdate {
        vm_id: "vm-3".into(),
        policy: EgressPolicy { rules },
        allow_existing: true,
    };

    let result = compile_egress_update(&update);
    assert_eq!(result.rules_applied, 2);
    assert!(result.nft_commands.iter().any(|c| c.contains("10.0.0.0/8")));
    assert!(result.nft_commands.iter().any(|c| c.contains("0.0.0.0/0")));
}
