//! Egress denial — security-critical (PRD §12.2, §12.6).
//!
//! "From inside the guest, attempt to reach non-allowlisted IPs/ports, raw
//! sockets, alternate DNS, IP spoofing, and ARP tricks — all must fail."
//! Requires KVM + net path (M8); gated.

#![cfg(test)]

#[test]
#[ignore = "needs KVM + net path (M8)"]
fn egress_denied_to_non_allowlisted() {
    // Placeholder.
}

#[test]
#[ignore = "needs KVM + egress path (M8)"]
fn raw_socket_blocked() {
    // Placeholder.
}

#[test]
#[ignore = "needs KVM + egress path (M8)"]
fn dns_rebinding_blocked() {
    // Placeholder.
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]
mod nft_allow_deny_e2e {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use vmm_net::egress::{EgressPolicy, EgressRule, Proto};
    use vmm_net::live_egress::{build_update_script, EgressUpdate};

    const CHILD_ENV: &str = "VMM_EGRESS_ALLOW_DENY_NETNS_CHILD";
    const TEST_NAME: &str =
        "nft_allow_deny_e2e::egress_allowlist_forward_chain_applies_in_test_netns";
    const ALLOWED_DEST: &str = "198.51.100.10";
    const DENIED_DEST: &str = "203.0.113.10";
    const CHAIN: &str = "vmm_egress_egress_allowdeny";

    #[test]
    #[ignore = "needs Linux+KVM feature, root/CAP_SYS_ADMIN, and nft; run on c8i"]
    fn egress_allowlist_forward_chain_applies_in_test_netns() {
        if std::env::var_os(CHILD_ENV).is_some() {
            run_netns_ruleset_assertions();
            return;
        }

        let current_exe = std::env::current_exe().expect("current test executable");
        let output = Command::new(current_exe)
            .args(["--exact", TEST_NAME, "--ignored", "--nocapture"])
            .env(CHILD_ENV, "1")
            .output()
            .expect("spawn isolated netns child");

        assert!(
            output.status.success(),
            "netns child failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_netns_ruleset_assertions() {
        unshare_netns();

        let update = EgressUpdate {
            vm_id: "egress_allowdeny".into(),
            policy: EgressPolicy {
                rules: vec![EgressRule {
                    cidr: ALLOWED_DEST.into(),
                    port: 443,
                    proto: Proto::Tcp,
                }],
            },
            allow_existing: false,
        };
        let script = build_update_script(&update);
        apply_nft_script(&script);
        let ruleset = list_nft_ruleset();

        assert!(
            ruleset.contains(&format!("chain {CHAIN}")),
            "per-VM egress chain missing from ruleset:\n{ruleset}"
        );
        assert!(
            ruleset.contains("hook forward") && ruleset.contains("policy drop"),
            "forward hook default-drop chain missing from ruleset:\n{ruleset}"
        );

        let lines: Vec<&str> = ruleset.lines().collect();
        let established_idx = lines
            .iter()
            .position(|line| {
                line.contains("ct state")
                    && line.contains("established")
                    && line.contains("related")
                    && line.contains("accept")
            })
            .unwrap_or_else(|| panic!("established/related accept missing:\n{ruleset}"));
        let allow_idx = lines
            .iter()
            .position(|line| {
                line.contains(ALLOWED_DEST)
                    && line.contains("tcp dport 443")
                    && line.contains("accept")
            })
            .unwrap_or_else(|| panic!("allowed destination rule missing:\n{ruleset}"));

        assert!(
            established_idx < allow_idx,
            "established/related accept must precede allow rules:\n{ruleset}"
        );
        assert!(
            !ruleset.contains(DENIED_DEST),
            "denied destination must be absent from allowlist rules:\n{ruleset}"
        );
    }

    fn unshare_netns() {
        // SAFETY: This ignored test runs in a short-lived child process; unshare
        // affects only that child before nft rules are applied and inspected.
        let rc = unsafe { libc::unshare(libc::CLONE_NEWNET) };
        assert_eq!(
            rc,
            0,
            "unshare(CLONE_NEWNET) failed: {}",
            std::io::Error::last_os_error()
        );
    }

    fn apply_nft_script(script: &str) {
        let mut child = Command::new("nft")
            .args(["-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn nft -f -");
        child
            .stdin
            .take()
            .expect("nft stdin")
            .write_all(script.as_bytes())
            .expect("write nft script");

        let output = child.wait_with_output().expect("wait nft -f -");
        assert!(
            output.status.success(),
            "nft apply failed\nscript:\n{script}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn list_nft_ruleset() -> String {
        let output = Command::new("nft")
            .args(["list", "ruleset"])
            .output()
            .expect("nft list ruleset");
        assert!(
            output.status.success(),
            "nft list ruleset failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("nft ruleset utf8")
    }
}
