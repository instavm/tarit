//! Request / response types for the control plane (1:1 model — no VM ids).

use crate::config::VmConfig;
use crate::state::VmStatus;
use serde::{Deserialize, Serialize};

/// Maximum accepted length-prefixed control-plane JSON frame size (16 MiB).
///
/// Every `[4-byte big-endian length][JSON body]` control frame on the VMM Unix
/// socket must be at or below this cap. The VMM server, the orchestrator client,
/// and the integration docs all reference this single constant so the wire
/// contract cannot drift between them.
pub const MAX_API_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSpec {
    pub config: VmConfig,
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
    },
    Restore {
        snapshot_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overlay: Option<String>,
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
    /// Return a cheap health/info snapshot of the VM (state, uptime, config).
    Status,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApiResponse {
    Ok,
    Snapshot {
        path: String,
    },
    Restored,
    Exec {
        exit_code: i32,
        stdout: String,
        stderr: String,
        duration_ms: u64,
    },
    EgressUpdated {
        rules_applied: usize,
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
        let r = ApiRequest::Snapshot { diff: true };
        let s = serde_json::to_string(&r).unwrap();
        let back: ApiRequest = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ApiRequest::Snapshot { diff } if diff));
    }

    #[test]
    fn request_restore_accepts_old_json_without_overlay() {
        let json = r#"{"op":"restore","snapshot_path":"/golden.snap"}"#;
        let back: ApiRequest = serde_json::from_str(json).unwrap();
        match back {
            ApiRequest::Restore {
                snapshot_path,
                overlay,
            } => {
                assert_eq!(snapshot_path, "/golden.snap");
                assert_eq!(overlay, None);
            }
            _ => panic!("expected restore"),
        }
    }

    #[test]
    fn request_restore_round_trips_with_overlay() {
        let r = ApiRequest::Restore {
            snapshot_path: "/golden.snap".into(),
            overlay: Some("/clones/a.cow".into()),
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
                overlay: Some(overlay)
            } if snapshot_path == "/golden.snap" && overlay == "/clones/a.cow"
        ));
    }

    #[test]
    fn response_variants_round_trip() {
        for r in [
            ApiResponse::Ok,
            ApiResponse::Snapshot { path: "/p".into() },
            ApiResponse::Restored,
            ApiResponse::Err { msg: "bad".into() },
        ] {
            let s = serde_json::to_string(&r).unwrap();
            let back: ApiResponse = serde_json::from_str(&s).unwrap();
            let _ = back;
        }
    }

    #[test]
    fn response_err_has_msg_field() {
        let r = ApiResponse::Err { msg: "nope".into() };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"msg\":\"nope\""));
        assert!(s.contains("\"status\":\"err\""));
    }
}
