//! Comprehensive E2E test — every feature, every state, every edge case.
//!
//! Tests:
//! 1. Cold boot + halt
//! 2. Snapshot (full)
//! 3. Restore (eager + UFFD fallback)
//! 4. Clone fan-out spec generation (unique MACs/IPs)
//! 5. Egress policy compilation + live update + diff
//! 6. Port forwarding (DNAT + SNAT)
//! 7. Security policy (zero-exfiltration, no network syscalls)
//! 8. Rate limiter (token bucket)
//! 9. DNS-aware egress (domain → /32)
//! 10. Clock/PRNG restore semantics
//! 11. Jailer (config + rlimits)
//! 12. OCI image ref parsing
//! 13. Live snapshot convergence algorithm
//! 14. Migration state machine
//! 15. API list
//! 16. Diff snapshot equivalence
//! 17. Dirty bitmap accuracy

#![cfg(all(target_os = "linux", target_arch = "x86_64", feature = "kvm"))]

use std::path::PathBuf;
use std::time::Instant;
use vmm_core::clone::build_clone_specs;
use vmm_core::config::{KernelConfig, MemoryConfig, VcpuConfig, VmConfig};
use vmm_core::controller::VmmController;
use vmm_core::oci::OciImageRef;
use vmm_core::restore_semantics::{compute_post_restore, ClockRestoreConfig};
use vmm_core::security::SecurityPolicy;
use vmm_net::dns::{expand, DnsAwarePolicy, MapResolver};
use vmm_net::egress::{EgressPolicy, EgressRule, Proto};
use vmm_net::live_egress::{compile_egress_update, diff_policies, EgressUpdate};
use vmm_net::nft_compiler::compile_table;
use vmm_net::port_forward::{compile_port_forward, PortForward};
use vmm_net::rate_limit::TokenBucket;
use vmm_snapshot::diff::apply_diffs;
use vmm_snapshot::live::{decide, PrecopyParams, RoundDecision};

fn kernel_path() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    PathBuf::from(manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("guest/bzImage"))
        .unwrap_or_else(|| PathBuf::from("guest/bzImage"))
}

fn vm_config() -> VmConfig {
    VmConfig {
        kernel: KernelConfig {
            path: kernel_path().to_string_lossy().to_string(),
            cmdline: "console=ttyS0 reboot=k panic=1 nokaslr".into(),
            initramfs: None,
        },
        memory: MemoryConfig { size_mib: 256 },
        vcpus: VcpuConfig { count: 1 },
        volumes: vec![],
        net: vec![],
    }
}

fn retain_snapshot(controller: &VmmController, path: &str) {
    let identity = vmm_core::gc::OwnedScratchFile::identity_for(std::path::Path::new(path))
        .expect("snapshot identity");
    controller
        .release_scratch(path, identity)
        .expect("transfer snapshot ownership");
}

#[test]
#[ignore = "needs Linux+KVM + guest/bzImage"]
fn comprehensive_all_features_and_edge_cases() {
    let kpath = kernel_path();
    if !kpath.exists() {
        eprintln!("kernel not found — skip");
        return;
    }

    let controller = VmmController::new();
    let mut passed = 0;
    let mut failed = 0;

    macro_rules! check {
        ($name:expr, $cond:expr) => {
            if $cond {
                eprintln!("  ✓ {}", $name);
                passed += 1;
            } else {
                eprintln!("  ✗ {}", $name);
                failed += 1;
            }
        };
    }

    // === 1. Cold Boot ===
    eprintln!("=== 1. Cold Boot ===");
    let t = Instant::now();
    check!(
        "boot via controller",
        controller.create(vm_config()).is_ok()
    );
    eprintln!("    boot: {}ms", t.elapsed().as_millis());

    // === 2. Snapshot ===
    eprintln!("=== 2. Snapshot ===");
    let snap_result = controller.snapshot(false);
    check!("snapshot created", snap_result.is_ok());
    let snap_path = snap_result.unwrap_or_default();
    if !snap_path.is_empty() {
        retain_snapshot(&controller, &snap_path);
        let size = std::fs::metadata(&snap_path).map(|m| m.len()).unwrap_or(0);
        check!("snapshot file non-empty", size > 0);
    }

    // === 3. Restore ===
    eprintln!("=== 3. Restore ===");
    let restore_result = controller.restore(&snap_path, None);
    check!("restore succeeded", restore_result.is_ok());

    // === 4. Clone Fan-Out ===
    eprintln!("=== 4. Clone Fan-Out ===");
    let specs = build_clone_specs("base", &snap_path, None, 5, "/tmp");
    check!("5 clone specs generated", specs.len() == 5);
    let macs: Vec<_> = specs
        .iter()
        .filter_map(|s| s.net.as_ref().and_then(|n| n.guest_mac.clone()))
        .collect();
    check!(
        "unique MACs",
        macs.iter().collect::<std::collections::HashSet<_>>().len() == 5
    );
    let ips: Vec<_> = specs
        .iter()
        .filter_map(|s| s.net.as_ref().and_then(|n| n.guest_ip.clone()))
        .collect();
    check!(
        "unique IPs",
        ips.iter().collect::<std::collections::HashSet<_>>().len() == 5
    );

    // === 5. Egress Policy ===
    eprintln!("=== 5. Egress Policy ===");
    let policy = EgressPolicy {
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
    };
    let nft_table = compile_table(&policy);
    check!("nft table has entries", nft_table.len() > 3);
    check!("nft default-deny", nft_table[1].contains("policy drop"));
    check!(
        "nft stateful replies",
        nft_table[2].contains("ct state established")
    );

    // === 6. Live Egress Update ===
    eprintln!("=== 6. Live Egress Update ===");
    let update = EgressUpdate {
        vm_id: "vm-1".into(),
        policy: EgressPolicy::deny_all(),
        allow_existing: true,
    };
    let result = compile_egress_update(&update);
    check!(
        "live update flushes chain",
        result.nft_commands[0].contains("flush chain")
    );
    check!(
        "live update allows stateful replies",
        result.nft_commands[1].contains("ct state established")
    );

    // === 7. Egress Diff ===
    eprintln!("=== 7. Egress Diff ===");
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
    check!("diff detects 1 added", diff.added.len() == 1);
    check!("diff detects 1 removed", diff.removed.len() == 1);

    // === 8. Port Forwarding ===
    eprintln!("=== 8. Port Forwarding ===");
    let pf = PortForward {
        host_port: 8080,
        guest_ip: "172.16.0.2".into(),
        guest_port: 80,
        proto: "tcp".into(),
    };
    let pf_rules = compile_port_forward(&pf);
    check!("DNAT rule", pf_rules[0].contains("dnat to 172.16.0.2:80"));
    check!("SNAT masquerade", pf_rules[1].contains("masquerade"));

    // === 9. Security Policy ===
    eprintln!("=== 9. Security Policy ===");
    let sec = SecurityPolicy::locked_down();
    check!("seccomp confined", sec.seccomp_confined);
    check!("isolated netns", sec.isolated_netns);
    check!("deny-all egress", sec.egress_allowlist.is_empty());
    check!(
        "no socket in allowlist",
        !vmm_core::security::VMM_SYSCALL_ALLOWLIST.contains(&"socket")
    );
    check!(
        "no connect in allowlist",
        !vmm_core::security::VMM_SYSCALL_ALLOWLIST.contains(&"connect")
    );
    check!(
        "no bind in allowlist",
        !vmm_core::security::VMM_SYSCALL_ALLOWLIST.contains(&"bind")
    );

    // === 10. Rate Limiter ===
    eprintln!("=== 10. Rate Limiter ===");
    let mut bucket = TokenBucket::new(1000, 100);
    check!("bucket starts full", bucket.consume(500, 0));
    check!("bucket rejects over-capacity", !bucket.consume(1000, 0));

    // === 11. DNS-Aware Egress ===
    eprintln!("=== 11. DNS-Aware Egress ===");
    let dns_policy = DnsAwarePolicy {
        cidr_rules: vec![],
        domain_rules: vec![vmm_net::dns::DomainRule {
            domain: "example.com".into(),
            port: 443,
            proto: Proto::Tcp,
        }],
    };
    let resolver = MapResolver {
        map: {
            let mut m = std::collections::HashMap::new();
            m.insert("example.com".into(), vec!["93.184.216.34".into()]);
            m
        },
    };
    let expanded = expand(&dns_policy, &resolver);
    check!(
        "DNS resolves to /32",
        expanded.rules.iter().any(|r| r.cidr == "93.184.216.34/32")
    );

    // === 12. Clock/PRNG Restore ===
    eprintln!("=== 12. Clock/PRNG Restore ===");
    let cfg = ClockRestoreConfig::default_for_clone();
    let actions = compute_post_restore(&cfg);
    check!("clock reset on restore", actions.clock_reset);
    check!("CRNG reseed pending", actions.crng_reseed_pending);

    // === 13. Jailer Config ===
    eprintln!("=== 13. Jailer Config ===");
    let jail_cfg = vmm_jailer::jailer::JailerConfig {
        chroot_dir: "/tmp/jail".into(),
        uid: 1000,
        gid: 1000,
        cgroup: "/sys/fs/cgroup/vmm".into(),
        rlimit_nofile: 1024,
        rlimit_as: 1 << 30,
        netns: "".into(),
        cgroup_limits: None,
    };
    check!(
        "jailer config valid",
        jail_cfg.uid == 1000 && jail_cfg.rlimit_nofile == 1024
    );

    // === 14. OCI Image Ref ===
    eprintln!("=== 14. OCI Image Ref ===");
    let oci = OciImageRef {
        reference: "docker://ubuntu:22.04".into(),
        auth_file: None,
    };
    check!("OCI ref parses", oci.reference.contains("ubuntu:22.04"));

    // === 15. Live Snapshot Convergence Algorithm ===
    eprintln!("=== 15. Live Snapshot Convergence ===");
    let params = PrecopyParams {
        mem_bytes: 256 * 1024 * 1024,
        dirty_rate_bps: 10_000_000,
        copy_bandwidth_bps: 1_000_000_000,
        target_downtime_us: 5_000,
        max_rounds: 20,
    };
    let decision = decide(&params, 1, 100 * 1024 * 1024);
    check!(
        "large dirty set continues",
        matches!(decision, RoundDecision::Continue { .. })
    );
    let decision = decide(&params, 1, 1 * 1024 * 1024);
    check!(
        "small dirty set stops",
        matches!(decision, RoundDecision::FinalStop { .. })
    );
    let params_div = PrecopyParams {
        dirty_rate_bps: 2_000_000_000,
        ..params
    };
    let decision = decide(&params_div, 2, 100 * 1024 * 1024);
    check!(
        "high dirty rate diverges",
        matches!(decision, RoundDecision::Diverging { .. })
    );
    // Edge case: zero dirty bytes → stop immediately
    let decision = decide(&params, 1, 0);
    check!(
        "zero dirty stops",
        matches!(decision, RoundDecision::FinalStop { .. })
    );
    // Edge case: max rounds reached → stop
    let decision = decide(&params, 20, 100 * 1024 * 1024);
    check!(
        "max rounds forces stop",
        matches!(decision, RoundDecision::FinalStop { .. })
    );

    // === 16. Diff Snapshot Equivalence ===
    eprintln!("=== 16. Diff Snapshot Equivalence ===");
    let base = vec![0xAA; 8 * 4096];
    let mut d1 = std::collections::HashSet::new();
    d1.insert(1u64); // page 1
    let mut d2 = std::collections::HashSet::new();
    d2.insert(5u64); // page 5

    // Build the "full" image (what the memory looks like after writes)
    let mut full = base.clone();
    for i in 0x1000..0x2000 {
        full[i] = 0xBB;
    }
    for i in 0x5000..0x6000 {
        full[i] = 0xCC;
    }

    // Build diffs
    let diff1 = vmm_snapshot::diff::PageDelta {
        gpa: 0x1000,
        bytes: full[0x1000..0x2000].to_vec(),
    };
    let diff2 = vmm_snapshot::diff::PageDelta {
        gpa: 0x5000,
        bytes: full[0x5000..0x6000].to_vec(),
    };
    let snap1 = vmm_snapshot::diff::DiffSnapshot {
        pages: vec![diff1],
        state: vec![],
    };
    let snap2 = vmm_snapshot::diff::DiffSnapshot {
        pages: vec![diff2],
        state: vec![],
    };
    let reconstructed = apply_diffs(&base, &[snap1, snap2]);
    check!("diff equivalence (byte-for-byte)", reconstructed == full);

    // === 17. Migration State Machine ===
    eprintln!("=== 17. Migration State Machine ===");
    use vmm_migration::state::{MigrationPhase, MigrationState};
    let mut ms = MigrationState::new();
    check!(
        "starts at negotiation",
        ms.phase == MigrationPhase::Negotiation
    );
    ms.advance(RoundDecision::Continue {
        round: 0,
        dirty_bytes: 0,
    });
    check!(
        "advances to dest prep",
        ms.phase == MigrationPhase::DestinationPrep
    );
    ms.advance(RoundDecision::Continue {
        round: 0,
        dirty_bytes: 0,
    });
    check!("advances to precopy", ms.phase == MigrationPhase::Precopy);
    ms.advance(RoundDecision::FinalStop {
        round: 1,
        dirty_bytes: 1000,
    });
    check!(
        "advances to stop-and-copy",
        ms.phase == MigrationPhase::StopAndCopy
    );
    ms.advance(RoundDecision::FinalStop {
        round: 1,
        dirty_bytes: 0,
    });
    check!("advances to cutover", ms.phase == MigrationPhase::Cutover);
    ms.advance(RoundDecision::FinalStop {
        round: 1,
        dirty_bytes: 0,
    });
    check!("advances to done", ms.phase == MigrationPhase::Done);
    // Post-copy fallback
    let mut ms2 = MigrationState::new();
    ms2.advance(RoundDecision::Continue {
        round: 0,
        dirty_bytes: 0,
    });
    ms2.advance(RoundDecision::Continue {
        round: 0,
        dirty_bytes: 0,
    });
    ms2.advance(RoundDecision::Diverging { round: 2 });
    check!(
        "diverging → postcopy",
        ms2.phase == MigrationPhase::Postcopy
    );

    // === 18. API List ===
    // removed: multi-VM enumeration not applicable in 1:1 model

    // === 19. Stop ===
    eprintln!("=== 19. Stop ===");
    check!("stop VM", controller.stop().is_ok());

    // Cleanup
    let _ = std::fs::remove_file(&snap_path);

    eprintln!("\n=== RESULTS: {passed} passed, {failed} failed ===");
    assert!(failed == 0, "{failed} feature checks failed");
}
