//! Request / response types for the control plane (1:1 model — no VM ids).

use crate::config::{NetConfig, VmConfig, VolumeConfig};
use crate::state::VmStatus;
use serde::{Deserialize, Serialize};

/// Maximum accepted length-prefixed control-plane JSON frame size (16 MiB).
///
/// Every `[4-byte big-endian length][JSON body]` control frame on the VMM Unix
/// socket must be at or below this cap. The VMM server, the orchestrator client,
/// and the integration docs all reference this single constant so the wire
/// contract cannot drift between them.
pub const MAX_API_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreMemoryPolicy {
    #[default]
    Auto,
    Eager,
    Lazy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSpec {
    pub config: VmConfig,
}

/// Stable Unix file identity used to transfer ownership of VMM scratch files.
///
/// The receiver must verify this identity before disarming cleanup; a path on
/// its own is never proof that it still names the artifact originally created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScratchIdentity {
    pub device: u64,
    pub inode: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_nanos: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestNetworkRepair {
    pub addr: String,
    pub prefix: u8,
    pub gateway: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns_servers: Vec<String>,
}

/// Trusted control-plane anchor for the private chunk manifest copied into the
/// VMM namespace. The manifest is small and verified eagerly; RAM bytes are
/// verified chunk-by-chunk as UFFD faults them in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryIntegrity {
    pub manifest_path: String,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ApiRequest {
    /// Boot the single VM with the given config.
    Create(VmSpec),
    Pause,
    Suspend,
    Resume,
    Snapshot {
        diff: bool,
        /// Take a live (pre-copy) snapshot: the guest keeps running and only
        /// blacks out for a sub-millisecond final stop. Defaults to false so
        /// older clients' requests still parse.
        #[serde(default)]
        live: bool,
    },
    /// Transfer one exact VMM-owned scratch file to the caller.
    ReleaseScratch {
        path: String,
        identity: ScratchIdentity,
    },
    Restore {
        snapshot_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        memory_integrity: Option<MemoryIntegrity>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overlay: Option<String>,
        /// Explicit replacement for snapshot-saved host network bindings.
        /// `Some([])` is valid only for a networkless snapshot. NIC addition or
        /// removal is unsupported because restored MMIO topology must match.
        /// `None` is accepted only when the snapshot itself has no NIC,
        /// preventing stale tap/IP reuse.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        net: Option<Vec<NetConfig>>,
        /// Replacement descriptors for every snapshot-saved inherited block
        /// device, in saved-device order. The VMM never reuses serialized raw
        /// descriptor numbers across processes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        volumes: Option<Vec<VolumeConfig>>,
        /// Restore memory backend policy. `auto` prefers UFFD-lazy restore for
        /// full snapshots and falls back to eager replay when needed; `eager`
        /// and `lazy` require that exact behavior.
        #[serde(default, skip_serializing_if = "is_default_restore_memory_policy")]
        memory_policy: RestoreMemoryPolicy,
    },
    RepairGuestNetwork {
        network: GuestNetworkRepair,
    },
    Stop,
    /// Execute a command in the guest.
    Exec {
        command: String,
        #[serde(default)]
        timeout_ms: u64,
    },
    /// Attach an interactive PTY stream in the guest. This switches the UDS
    /// connection to PTY stream framing and does not produce an ApiResponse.
    AttachPty {
        cols: u16,
        rows: u16,
        shell: Option<String>,
    },
    /// Update egress policy on a running VM (live, no restart).
    UpdateEgress {
        allowlist: Vec<String>,
        #[serde(default)]
        allow_existing: bool,
    },
    /// Set the traditional virtio-balloon target. This is best-effort reclaim,
    /// not a hard host memory limit.
    SetBalloon {
        target_mib: u64,
    },
    /// Read target/actual balloon state reported through virtio config space.
    Balloon,
    /// Return a cheap health/info snapshot of the VM (state, uptime, config).
    Status,
}

fn is_default_restore_memory_policy(policy: &RestoreMemoryPolicy) -> bool {
    *policy == RestoreMemoryPolicy::Auto
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveSnapshotTermination {
    Converged,
    Diverging,
    Timeout,
    MaxRounds,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveSnapshotStats {
    pub rounds: u32,
    pub pages_copied: u64,
    pub final_dirty_pages: u64,
    pub elapsed_us: u64,
    pub downtime_us: u64,
    pub termination: LiveSnapshotTermination,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApiResponse {
    Ok,
    Snapshot {
        path: String,
        /// Disk upper captured at the same atomic boundary as a live memory
        /// snapshot. Absent for memory-only and legacy snapshots.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overlay_path: Option<String>,
        /// VMM-generated chunk hashes for the exact live RAM snapshot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        integrity_path: Option<String>,
        /// Structured pre-copy outcome. Present only for live snapshots.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        live_stats: Option<LiveSnapshotStats>,
    },
    Restored,
    GuestNetworkRepaired,
    Exec {
        exit_code: i32,
        stdout: String,
        stderr: String,
        duration_ms: u64,
    },
    EgressUpdated {
        rules_applied: usize,
    },
    Balloon {
        target_mib: u64,
        actual_mib: u64,
        target_pages: u32,
        actual_pages: u32,
    },
    /// Health/info snapshot (response to `Status`).
    #[serde(rename = "vm_status")]
    Status(VmStatus),
    Err {
        msg: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{KernelConfig, MemoryConfig, VcpuConfig, VmConfig, VolumeConfig};

    fn cfg() -> VmConfig {
        VmConfig {
            kernel: KernelConfig {
                path: "/k".into(),
                cmdline: "console=ttyS0".into(),
                initramfs: None,
            },
            memory: MemoryConfig { size_mib: 256 },
            vcpus: VcpuConfig { count: 1 },
            volumes: vec![],
            net: vec![],
        }
    }

    #[test]
    fn request_create_round_trips() {
        let r = ApiRequest::Create(VmSpec { config: cfg() });
        let s = serde_json::to_string(&r).unwrap();
        let back: ApiRequest = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ApiRequest::Create(_)));
    }

    #[test]
    fn request_create_accepts_volume_without_overlay() {
        let json = r#"{"op":"create","config":{"kernel":{"path":"/k","cmdline":"","initramfs":null},"memory":{"size_mib":64},"vcpus":{"count":1},"volumes":[{"path":"/base.img","read_only":true}],"net":[]}}"#;
        let back: ApiRequest = serde_json::from_str(json).unwrap();
        match back {
            ApiRequest::Create(spec) => assert_eq!(spec.config.volumes[0].overlay, None),
            _ => panic!("expected create"),
        }
    }

    #[test]
    fn request_create_round_trips_volume_overlay() {
        let mut config = cfg();
        config.volumes.push(VolumeConfig {
            path: "/base.img".into(),
            read_only: true,
            overlay: Some("/overlay.cow".into()),
            inherited_fd: None,
        });
        let r = ApiRequest::Create(VmSpec { config });
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"overlay\":\"/overlay.cow\""));
        let back: ApiRequest = serde_json::from_str(&s).unwrap();
        match back {
            ApiRequest::Create(spec) => {
                assert_eq!(
                    spec.config.volumes[0].overlay.as_deref(),
                    Some("/overlay.cow")
                );
            }
            _ => panic!("expected create"),
        }
    }

    #[test]
    fn request_stop_round_trips() {
        let r = ApiRequest::Stop;
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"op\":\"stop\""));
        let back: ApiRequest = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ApiRequest::Stop));
    }

    #[test]
    fn request_status_round_trips() {
        let r = ApiRequest::Status;
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"op\":\"status\""));
        let back: ApiRequest = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ApiRequest::Status));
    }

    #[test]
    fn request_suspend_round_trips() {
        let r = ApiRequest::Suspend;
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"op\":\"suspend\""));
        let back: ApiRequest = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ApiRequest::Suspend));
    }

    #[test]
    fn request_attach_pty_round_trips() {
        let r = ApiRequest::AttachPty {
            cols: 100,
            rows: 30,
            shell: Some("/bin/sh".into()),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"op\":\"attach_pty\""));
        let back: ApiRequest = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            ApiRequest::AttachPty {
                cols: 100,
                rows: 30,
                shell: Some(_)
            }
        ));
    }

    #[test]
    fn response_status_round_trips() {
        use crate::state::{VmState, VmStatus};
        let st = VmStatus {
            state: VmState::Running,
            uptime_ms: 1234,
            vcpus: 2,
            mem_mib: 512,
            volumes: 1,
            nets: 1,
            kernel: "/vmlinux".into(),
            vcpu_alive: true,
        };
        let r = ApiResponse::Status(st.clone());
        let s = serde_json::to_string(&r).unwrap();
        // Internally-tagged: the tag key is "status" and the Status variant is
        // renamed to "vm_status" (avoiding the awkward {"status":"status"}),
        // with VmStatus's fields flattened alongside.
        assert!(s.contains("\"status\":\"vm_status\""));
        assert!(s.contains("\"state\":\"running\""));
        let back: ApiResponse = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ApiResponse::Status(b) if b == st));
    }

    #[test]
    fn request_snapshot_round_trips_with_diff_flag() {
        let r = ApiRequest::Snapshot {
            diff: true,
            live: false,
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: ApiRequest = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ApiRequest::Snapshot { diff, live } if diff && !live));
    }

    #[test]
    fn request_snapshot_live_round_trips() {
        let r = ApiRequest::Snapshot {
            diff: false,
            live: true,
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: ApiRequest = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ApiRequest::Snapshot { diff, live } if !diff && live));
    }

    #[test]
    fn request_snapshot_without_live_field_still_parses() {
        // Wire compat: requests from clients predating the `live` flag.
        let back: ApiRequest = serde_json::from_str(r#"{"op":"snapshot","diff":true}"#).unwrap();
        assert!(matches!(back, ApiRequest::Snapshot { diff, live } if diff && !live));
    }

    #[test]
    fn request_release_scratch_round_trips_with_exact_identity() {
        let r = ApiRequest::ReleaseScratch {
            path: "/snapshots/vmm-snap-1-2.snap".into(),
            identity: ScratchIdentity {
                device: 1,
                inode: 2,
                created_secs: Some(3),
                created_nanos: Some(4),
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: ApiRequest = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            ApiRequest::ReleaseScratch { path, identity }
                if path == "/snapshots/vmm-snap-1-2.snap"
                    && identity.device == 1
                    && identity.inode == 2
        ));
    }

    #[test]
    fn request_restore_accepts_old_json_without_overlay() {
        let json = r#"{"op":"restore","snapshot_path":"/golden.snap"}"#;
        let back: ApiRequest = serde_json::from_str(json).unwrap();
        match back {
            ApiRequest::Restore {
                snapshot_path,
                memory_integrity,
                overlay,
                net,
                volumes,
                memory_policy,
            } => {
                assert_eq!(snapshot_path, "/golden.snap");
                assert!(memory_integrity.is_none());
                assert_eq!(overlay, None);
                assert!(net.is_none());
                assert!(volumes.is_none());
                assert_eq!(memory_policy, RestoreMemoryPolicy::Auto);
            }
            _ => panic!("expected restore"),
        }
    }

    #[test]
    fn request_restore_round_trips_with_overlay() {
        let r = ApiRequest::Restore {
            snapshot_path: "/golden.snap".into(),
            memory_integrity: None,
            overlay: Some("/clones/a.cow".into()),
            net: None,
            volumes: None,
            memory_policy: RestoreMemoryPolicy::Auto,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(
            s,
            r#"{"op":"restore","snapshot_path":"/golden.snap","overlay":"/clones/a.cow"}"#
        );
        let back: ApiRequest = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            ApiRequest::Restore {
                snapshot_path,
                memory_integrity: None,
                overlay: Some(overlay),
                net: None,
                volumes: None,
                memory_policy: RestoreMemoryPolicy::Auto,
            } if snapshot_path == "/golden.snap" && overlay == "/clones/a.cow"
        ));
    }

    #[test]
    fn request_restore_round_trips_with_explicit_network_rebind() {
        let r = ApiRequest::Restore {
            snapshot_path: "/golden.snap".into(),
            memory_integrity: None,
            overlay: None,
            net: Some(vec![NetConfig {
                tap: "tap-new".into(),
                guest_mac: Some("02:00:00:00:00:02".into()),
                guest_ip: Some("10.0.0.3".into()),
                port_forwards: Vec::new(),
            }]),
            volumes: None,
            memory_policy: RestoreMemoryPolicy::Auto,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: ApiRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            ApiRequest::Restore {
                net: Some(net),
                memory_policy: RestoreMemoryPolicy::Auto,
                ..
            }
                if net.len() == 1 && net[0].tap == "tap-new"
        ));
    }

    #[test]
    fn request_restore_round_trips_non_default_memory_policy() {
        let r = ApiRequest::Restore {
            snapshot_path: "/golden.snap".into(),
            memory_integrity: None,
            overlay: None,
            net: None,
            volumes: None,
            memory_policy: RestoreMemoryPolicy::Lazy,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""memory_policy":"lazy""#));
        let back: ApiRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            ApiRequest::Restore {
                memory_policy: RestoreMemoryPolicy::Lazy,
                ..
            }
        ));
    }

    #[test]
    fn request_guest_network_repair_round_trips() {
        let request = ApiRequest::RepairGuestNetwork {
            network: GuestNetworkRepair {
                addr: "10.0.0.2".into(),
                prefix: 30,
                gateway: "10.0.0.1".into(),
                dns_servers: vec!["1.1.1.1".into(), "8.8.8.8".into()],
            },
        };
        let json = serde_json::to_string(&request).unwrap();
        let back: ApiRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            ApiRequest::RepairGuestNetwork { network }
                if network.addr == "10.0.0.2"
                    && network.prefix == 30
                    && network.gateway == "10.0.0.1"
                    && network.dns_servers.len() == 2
        ));
    }

    #[test]
    fn response_variants_round_trip() {
        for r in [
            ApiResponse::Ok,
            ApiResponse::Snapshot {
                path: "/p".into(),
                overlay_path: None,
                integrity_path: None,
                live_stats: None,
            },
            ApiResponse::Restored,
            ApiResponse::GuestNetworkRepaired,
            ApiResponse::Err { msg: "bad".into() },
        ] {
            let s = serde_json::to_string(&r).unwrap();
            let back: ApiResponse = serde_json::from_str(&s).unwrap();
            let _ = back;
        }
    }

    #[test]
    fn live_snapshot_response_round_trips_structured_stats() {
        let stats = LiveSnapshotStats {
            rounds: 7,
            pages_copied: 123_456,
            final_dirty_pages: 32_768,
            elapsed_us: 30_125_000,
            downtime_us: 357_000,
            termination: LiveSnapshotTermination::Timeout,
        };
        let response = ApiResponse::Snapshot {
            path: "/snapshots/live.snap".into(),
            overlay_path: Some("/snapshots/live.overlay".into()),
            integrity_path: Some("/snapshots/live.integrity".into()),
            live_stats: Some(stats.clone()),
        };

        let json = serde_json::to_string(&response).unwrap();
        let back: ApiResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            ApiResponse::Snapshot {
                live_stats: Some(actual),
                ..
            } if actual == stats
        ));
    }

    #[test]
    fn response_err_has_msg_field() {
        let r = ApiResponse::Err { msg: "nope".into() };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"msg\":\"nope\""));
        assert!(s.contains("\"status\":\"err\""));
    }
}
