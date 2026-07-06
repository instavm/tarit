//! DNS-aware egress allowlisting (Phase 6).
//!
//! DNS-aware allowlisting for domain-based policies: resolve
//! allowed domains and program the resulting IPs into nftables sets
//! dynamically. Pair with a controlled resolver so the guest can't smuggle
//! traffic via arbitrary DNS.
//!
//! This module resolves an allowlist of domain names to IPs (via the host's
//! resolver) and produces the expanded `EgressRule` set. The actual DNS
//! resolution is a host call; the *policy expansion* logic is pure and
//! unit-testable.

use crate::egress::{EgressPolicy, EgressRule, Proto};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A domain-based allowlist entry (resolved to IPs at policy-compile time).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DomainRule {
    pub domain: String,
    pub port: u16,
    pub proto: Proto,
}

/// A policy spec that may contain both CIDR rules and domain rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DnsAwarePolicy {
    pub cidr_rules: Vec<EgressRule>,
    pub domain_rules: Vec<DomainRule>,
}

/// The resolver trait — abstracts `getaddrinfo` so the expansion is testable
/// without real DNS.
pub trait DnsResolver {
    fn resolve(&self, domain: &str) -> Vec<String>;
}

/// Expand a `DnsAwarePolicy` into a flat `EgressPolicy` (CIDRs only) by
/// resolving each domain rule via `resolver`. Domains that fail to resolve
/// are silently dropped (the rule effectively doesn't apply).
pub fn expand(policy: &DnsAwarePolicy, resolver: &dyn DnsResolver) -> EgressPolicy {
    let mut out = EgressPolicy::deny_all();
    for r in &policy.cidr_rules {
        out.add(r.clone());
    }
    for d in &policy.domain_rules {
        for ip in resolver.resolve(&d.domain) {
            out.add(EgressRule {
                cidr: format!("{ip}/32"),
                port: d.port,
                proto: d.proto,
            });
        }
    }
    out
}

/// A test/mock resolver backed by a HashMap.
#[derive(Debug, Default, Clone)]
pub struct MapResolver {
    pub map: HashMap<String, Vec<String>>,
}

impl DnsResolver for MapResolver {
    fn resolve(&self, domain: &str) -> Vec<String> {
        self.map.get(domain).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_cidr_only_passes_through() {
        let policy = DnsAwarePolicy {
            cidr_rules: vec![EgressRule {
                cidr: "10.0.0.0/8".into(),
                port: 443,
                proto: Proto::Tcp,
            }],
            domain_rules: vec![],
        };
        let out = expand(&policy, &MapResolver::default());
        assert_eq!(out.rules.len(), 1);
        assert_eq!(out.rules[0].cidr, "10.0.0.0/8");
    }

    #[test]
    fn expand_resolves_domains_to_ips() {
        let policy = DnsAwarePolicy {
            cidr_rules: vec![],
            domain_rules: vec![DomainRule {
                domain: "example.com".into(),
                port: 443,
                proto: Proto::Tcp,
            }],
        };
        let resolver = MapResolver {
            map: {
                let mut m = HashMap::new();
                m.insert("example.com".into(), vec!["93.184.216.34".into()]);
                m
            },
        };
        let out = expand(&policy, &resolver);
        assert_eq!(out.rules.len(), 1);
        assert_eq!(out.rules[0].cidr, "93.184.216.34/32");
        assert_eq!(out.rules[0].port, 443);
    }

    #[test]
    fn unresolvable_domain_dropped() {
        let policy = DnsAwarePolicy {
            cidr_rules: vec![],
            domain_rules: vec![DomainRule {
                domain: "no.such.domain".into(),
                port: 80,
                proto: Proto::Tcp,
            }],
        };
        let out = expand(&policy, &MapResolver::default());
        assert!(out.rules.is_empty());
    }

    #[test]
    fn domain_with_multiple_ips_emits_multiple_rules() {
        let policy = DnsAwarePolicy {
            cidr_rules: vec![],
            domain_rules: vec![DomainRule {
                domain: "cdn.example".into(),
                port: 443,
                proto: Proto::Tcp,
            }],
        };
        let resolver = MapResolver {
            map: {
                let mut m = HashMap::new();
                m.insert(
                    "cdn.example".into(),
                    vec!["1.2.3.4".into(), "5.6.7.8".into()],
                );
                m
            },
        };
        let out = expand(&policy, &resolver);
        assert_eq!(out.rules.len(), 2);
    }

    #[test]
    fn mixed_cidr_and_domain_combines() {
        let policy = DnsAwarePolicy {
            cidr_rules: vec![EgressRule {
                cidr: "0.0.0.0/0".into(),
                port: 0,
                proto: Proto::Any,
            }],
            domain_rules: vec![DomainRule {
                domain: "x.example".into(),
                port: 80,
                proto: Proto::Tcp,
            }],
        };
        let resolver = MapResolver {
            map: {
                let mut m = HashMap::new();
                m.insert("x.example".into(), vec!["10.0.0.1".into()]);
                m
            },
        };
        let out = expand(&policy, &resolver);
        assert_eq!(out.rules.len(), 2);
    }
}
