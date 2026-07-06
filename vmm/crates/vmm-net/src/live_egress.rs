//! Live egress policy update — update nftables rules while a VM is running.
//!
//! Egress is default-deny; only an explicit per-VM/per-session
//! allowlist of CIDRs/ports is allowed. This module supports updating that
//! allowlist on a running VM without restarting it.
//!
//! The approach: each VM's egress rules live in a per-VM nftables chain
//! (e.g., `vmm_egress_vm-1`). Updating the policy = flush the chain +
//! re-apply the new rules. The guest sees the new policy immediately —
//! no VM restart needed. Stateful reply traffic is always accepted before the
//! allowlist, so permitted outbound connections can receive responses.

use crate::egress::{EgressPolicy, EgressRule};

#[cfg(test)]
use crate::egress::Proto;
use crate::nft_compiler::compile_to_nft;
use serde::{Deserialize, Serialize};

/// A live egress policy update for a running VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgressUpdate {
    pub vm_id: String,
    /// The new policy (replaces the old one entirely).
    pub policy: EgressPolicy,
    /// Whether callers should preserve existing conntrack state during the
    /// update. This compiler does not render that out-of-band decision; reply
    /// traffic is always accepted with an established/related rule because
    /// otherwise permitted outbound flows cannot receive replies.
    #[serde(default)]
    pub allow_existing: bool,
}

/// Result of applying a live egress update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgressUpdateResult {
    pub vm_id: String,
    pub rules_applied: usize,
    pub rules_dropped: Vec<String>,
    /// The nftables commands to execute (for the orchestrator to apply).
    pub nft_commands: Vec<String>,
}

/// Compile a live egress update into nftables commands.
///
/// Generates:
/// 1. `flush chain inet vmm vmm_egress_<vm_id>` (clear old rules)
/// 2. `add rule ... ct state established,related accept`
/// 3. New allow rules from the policy
/// 4. The chain's default policy remains `drop` (deny-all)
pub fn compile_egress_update(update: &EgressUpdate) -> EgressUpdateResult {
    let chain_name = format!("vmm_egress_{}", nft_safe_id(&update.vm_id));
    let mut commands = Vec::new();

    // 1. Flush the existing chain (remove all old rules).
    commands.push(format!("flush chain inet vmm {chain_name}"));

    // 2. Always allow replies for connections initiated by allowlisted egress.
    commands.push(format!(
        "add rule inet vmm {chain_name} ct state established,related accept"
    ));

    // 3. Add new allow rules.
    let allow_rules = compile_to_nft(&update.policy, &chain_name);
    commands.extend(allow_rules);

    EgressUpdateResult {
        vm_id: update.vm_id.clone(),
        rules_applied: update.policy.rules.len(),
        rules_dropped: vec![],
        nft_commands: commands,
    }
}

/// Compute the diff between an old and new policy — which rules are added
/// and which are removed. Useful for audit logging and minimal rule updates
/// (instead of flush + re-add everything).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyDiff {
    pub added: Vec<EgressRule>,
    pub removed: Vec<EgressRule>,
}

/// Error applying an egress update via `nft`.
#[derive(Debug, thiserror::Error)]
pub enum EgressApplyError {
    #[error("invalid egress rule: {0}")]
    Invalid(String),
    #[error("spawn nft: {0}")]
    Spawn(String),
    #[error("nft exited {code}: {stderr}")]
    Failed { code: i32, stderr: String },
}

/// Normalize an identifier (VM id) to a safe nftables identifier component.
///
/// nft unquoted identifiers are `[a-zA-Z_][a-zA-Z0-9_]*`; a raw VM id like
/// `vm-1` is invalid, and (critically) a VM id containing newlines/semicolons
/// fed to `nft -f -` would let an attacker inject arbitrary nft commands and
/// bypass the egress policy. Map every non-`[A-Za-z0-9_]` char to `_`, and
/// fall back to `default` when empty. The chain is always `vmm_egress_<id>`, so
/// the result always begins with a letter.
fn nft_safe_id(id: &str) -> String {
    let out: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "default".to_string()
    } else {
        out
    }
}

/// True if `s` is a bare IP or `IP/prefix` CIDR. Anything else (including
/// injection attempts with spaces, newlines, or nft keywords) fails to parse
/// and is rejected before it can reach `nft`.
fn valid_cidr(s: &str) -> bool {
    let (ip, prefix) = match s.split_once('/') {
        Some((ip, p)) => (ip, Some(p)),
        None => (s, None),
    };
    let addr = match ip.parse::<std::net::IpAddr>() {
        Ok(a) => a,
        Err(_) => return false,
    };
    match prefix {
        None => true,
        Some(p) => matches!(p.parse::<u8>(), Ok(n) if n <= if addr.is_ipv4() { 32 } else { 128 }),
    }
}

/// Build the atomic `nft -f -` batch script for a live update.
///
/// Ensures the per-VM table + chain exist (idempotent `add table`/`add chain`
/// with a default-drop policy), then the flush + allow rules from
/// [`compile_egress_update`]. Ordering matters: the chain must exist before the
/// `flush chain` in the update, so the first-ever apply does not fail. The
/// chain id is sanitized ([`nft_safe_id`]); callers must still validate rule
/// CIDRs (see [`apply_egress_update`]) before executing the script. Pure
/// string-building — host-agnostic and unit-testable without nft or a netns.
pub fn build_update_script(update: &EgressUpdate) -> String {
    let chain_name = format!("vmm_egress_{}", nft_safe_id(&update.vm_id));
    let mut commands = vec![
        "add table inet vmm".to_string(),
        format!(
            "add chain inet vmm {chain_name} {{ type filter hook forward priority 0; policy drop; }}"
        ),
    ];
    commands.extend(compile_egress_update(update).nft_commands);
    let mut script = commands.join("\n");
    script.push('\n');
    script
}

/// Apply a live egress update by piping the batch script to `nft -f -`.
///
/// Every rule's CIDR is validated first ([`valid_cidr`]); an invalid CIDR is
/// rejected before any `nft` execution so a malformed or malicious rule can
/// neither inject nft commands nor leave the chain in a half-applied state.
///
/// SAFETY / SECURITY: this programs the netfilter `forward` hook with a
/// default-drop policy, so it filters the guest's forwarded (routed) egress
/// rather than only the host's locally generated traffic. It MUST only be
/// called from a process that has entered the per-VM network namespace (the
/// jailed `vmm serve`); running it in the host's init netns would drop the
/// host's forwarded traffic. Callers are responsible
/// for that netns isolation (see `jailer-serve`).
pub fn apply_egress_update(update: &EgressUpdate) -> Result<EgressUpdateResult, EgressApplyError> {
    for r in &update.policy.rules {
        if !valid_cidr(&r.cidr) {
            return Err(EgressApplyError::Invalid(format!("bad CIDR: {:?}", r.cidr)));
        }
    }
    let script = build_update_script(update);
    run_nft(&script)?;
    Ok(compile_egress_update(update))
}

/// Feed an nft batch script to `nft -f -` and check the exit status.
fn run_nft(script: &str) -> Result<(), EgressApplyError> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("nft")
        .arg("-f")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| EgressApplyError::Spawn(e.to_string()))?;
    child
        .stdin
        .take()
        .ok_or_else(|| EgressApplyError::Spawn("no stdin".into()))?
        .write_all(script.as_bytes())
        .map_err(|e| EgressApplyError::Spawn(e.to_string()))?;
    let out = child
        .wait_with_output()
        .map_err(|e| EgressApplyError::Spawn(e.to_string()))?;
    if !out.status.success() {
        return Err(EgressApplyError::Failed {
            code: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(())
}

/// Compute the diff between two policies.
pub fn diff_policies(old: &EgressPolicy, new: &EgressPolicy) -> PolicyDiff {
    let old_set: std::collections::HashSet<_> = old.rules.iter().cloned().collect();
    let new_set: std::collections::HashSet<_> = new.rules.iter().cloned().collect();

    let added: Vec<_> = new_set.difference(&old_set).cloned().collect();
    let removed: Vec<_> = old_set.difference(&new_set).cloned().collect();

    PolicyDiff { added, removed }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(cidr: &str, port: u16, proto: Proto) -> EgressRule {
        EgressRule {
            cidr: cidr.into(),
            port,
            proto,
        }
    }

    #[test]
    fn compile_update_flushes_then_adds() {
        let update = EgressUpdate {
            vm_id: "vm-1".into(),
            policy: EgressPolicy {
                rules: vec![rule("10.0.0.0/8", 443, Proto::Tcp)],
            },
            allow_existing: false,
        };
        let result = compile_egress_update(&update);
        assert!(result.nft_commands[0].contains("flush chain"));
        assert!(result.nft_commands[1].contains("ct state established,related accept"));
        assert!(result.nft_commands[2].contains("ip daddr 10.0.0.0/8"));
        assert_eq!(result.rules_applied, 1);
    }

    #[test]
    fn compile_update_always_adds_conntrack() {
        let update = EgressUpdate {
            vm_id: "vm-1".into(),
            policy: EgressPolicy::deny_all(),
            allow_existing: false,
        };
        let result = compile_egress_update(&update);
        assert!(result.nft_commands[0].contains("flush chain"));
        assert!(result.nft_commands[1].contains("ct state established"));
    }

    #[test]
    fn diff_detects_added_and_removed() {
        let old = EgressPolicy {
            rules: vec![
                rule("10.0.0.0/8", 443, Proto::Tcp),
                rule("8.8.8.8/32", 53, Proto::Udp),
            ],
        };
        let new = EgressPolicy {
            rules: vec![
                rule("10.0.0.0/8", 443, Proto::Tcp),
                rule("1.1.1.1/32", 80, Proto::Tcp),
            ],
        };
        let diff = diff_policies(&old, &new);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.added[0].cidr, "1.1.1.1/32");
        assert_eq!(diff.removed[0].cidr, "8.8.8.8/32");
    }

    #[test]
    fn diff_no_change_returns_empty() {
        let policy = EgressPolicy {
            rules: vec![rule("0.0.0.0/0", 0, Proto::Any)],
        };
        let diff = diff_policies(&policy, &policy);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn update_serializes_round_trip() {
        let update = EgressUpdate {
            vm_id: "vm-1".into(),
            policy: EgressPolicy::allow_all(),
            allow_existing: true,
        };
        let s = serde_json::to_string(&update).unwrap();
        let back: EgressUpdate = serde_json::from_str(&s).unwrap();
        assert_eq!(back.vm_id, update.vm_id);
        assert!(back.allow_existing);
    }

    #[test]
    fn build_update_script_creates_table_and_chain_before_flush() {
        // The chain must be created before the flush, or the first-ever apply
        // fails on a nonexistent chain. Assert that ordering.
        let update = EgressUpdate {
            vm_id: "vm1".into(),
            policy: EgressPolicy {
                rules: vec![rule("10.0.0.0/8", 443, Proto::Tcp)],
            },
            allow_existing: false,
        };
        let script = build_update_script(&update);
        let lines: Vec<&str> = script.lines().collect();
        assert_eq!(lines[0], "add table inet vmm");
        assert!(lines[1].starts_with("add chain inet vmm vmm_egress_vm1"));
        assert!(lines[1].contains("policy drop"));
        let add_chain = lines
            .iter()
            .position(|l| l.starts_with("add chain"))
            .unwrap();
        let flush = lines
            .iter()
            .position(|l| l.starts_with("flush chain"))
            .unwrap();
        let stateful = lines
            .iter()
            .position(|l| l.contains("ct state established,related accept"))
            .unwrap();
        let allow = lines
            .iter()
            .position(|l| l.contains("tcp dport 443 accept"))
            .unwrap();
        assert!(
            add_chain < flush,
            "chain must be created before it is flushed"
        );
        assert!(
            flush < stateful && stateful < allow,
            "stateful reply rule must be the first rule after flush"
        );
        assert!(script.ends_with('\n'));
        // The allow rule is present.
        assert!(lines.iter().any(|l| l.contains("tcp dport 443 accept")));
    }

    #[test]
    fn nft_safe_id_sanitizes_and_defaults() {
        assert_eq!(nft_safe_id("vm-1"), "vm_1");
        assert_eq!(nft_safe_id(""), "default");
        // Injection attempt: newline + nft command is neutralized to underscores.
        let evil = nft_safe_id("x\nadd rule inet vmm c accept;");
        assert!(!evil.contains('\n') && !evil.contains(';') && !evil.contains(' '));
    }

    #[test]
    fn build_update_script_uses_sanitized_chain_consistently() {
        // A hyphenated / hostile vm_id must produce the SAME sanitized chain in
        // the add-chain, flush, and add-rule lines (no mismatch, no injection).
        let update = EgressUpdate {
            vm_id: "vm-1".into(),
            policy: EgressPolicy {
                rules: vec![rule("10.0.0.0/8", 443, Proto::Tcp)],
            },
            allow_existing: false,
        };
        let script = build_update_script(&update);
        assert!(
            !script.contains("vmm_egress_vm-1"),
            "hyphen must be sanitized"
        );
        assert_eq!(script.matches("vmm_egress_vm_1").count(), 4); // add chain + flush + stateful + allow
    }

    #[test]
    fn valid_cidr_accepts_ip_and_rejects_injection() {
        assert!(valid_cidr("10.0.0.0/8"));
        assert!(valid_cidr("8.8.8.8"));
        assert!(valid_cidr("2001:db8::/32"));
        assert!(!valid_cidr("10.0.0.0/33")); // prefix out of range
        assert!(!valid_cidr("not-an-ip"));
        assert!(!valid_cidr("10.0.0.0/8 accept; add rule inet vmm c accept")); // injection
        assert!(!valid_cidr("1.2.3.4\nflush ruleset"));
    }

    #[test]
    fn apply_rejects_bad_cidr_before_running_nft() {
        // apply_egress_update must reject an invalid CIDR (returning Invalid)
        // rather than shelling out to nft with an injectable script.
        let update = EgressUpdate {
            vm_id: "vm1".into(),
            policy: EgressPolicy {
                rules: vec![rule("1.2.3.4; flush ruleset", 0, Proto::Any)],
            },
            allow_existing: false,
        };
        let err = apply_egress_update(&update).unwrap_err();
        assert!(matches!(err, EgressApplyError::Invalid(_)));
    }
}
