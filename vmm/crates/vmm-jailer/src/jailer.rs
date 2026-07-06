//! Jailer config + entry point (scaffold; full impl in M9).

use crate::cgroups::CgroupLimits;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum JailerError {
    #[error("setup: {0}")]
    Setup(String),
    #[error("namespace: {0}")]
    Namespace(String),
    #[error("privilege drop: {0}")]
    PrivDrop(String),
    #[error("unimplemented: {0}")]
    Unimplemented(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JailerConfig {
    pub chroot_dir: String,
    pub uid: u32,
    pub gid: u32,
    /// cgroup v2 path under `/sys/fs/cgroup/` (e.g. `/sys/fs/cgroup/vmm/vm-1`).
    /// If non-empty, the jailer creates this cgroup, applies limits, and
    /// adds the VMM process to it.
    pub cgroup: String,
    /// cgroup v2 resource limits (CPU, memory, IO, PIDs).
    /// Applied to the cgroup path above before dropping privileges.
    #[serde(default)]
    pub cgroup_limits: Option<CgroupLimits>,
    /// Max file descriptors for the jailed process.
    pub rlimit_nofile: u64,
    /// Max address space bytes (RLIMIT_AS).
    pub rlimit_as: u64,
    /// Network namespace path (created by vmm-net).
    pub netns: String,
}

pub struct Jailer {
    pub cfg: JailerConfig,
}

impl Jailer {
    pub fn new(cfg: JailerConfig) -> Self {
        Self { cfg }
    }

    /// Run the jailer: unshare namespaces, chroot, set rlimits, and drop
    /// privileges. This must never silently succeed without confinement.
    pub fn run(&self) -> Result<(), JailerError> {
        #[cfg(target_os = "linux")]
        {
            crate::executor::jail(&self.cfg)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(JailerError::Unimplemented(
                "jailer confinement is only implemented on Linux".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> JailerConfig {
        JailerConfig {
            chroot_dir: "/srv/jail".into(),
            uid: 1000,
            gid: 1000,
            cgroup: "/sys/fs/cgroup/vmm".into(),
            rlimit_nofile: 1024,
            rlimit_as: 1 << 30,
            netns: "/var/run/netns/vmm0".into(),
            cgroup_limits: None,
        }
    }

    #[test]
    fn config_round_trips_json() {
        let c = cfg();
        let s = serde_json::to_string(&c).unwrap();
        let back: JailerConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.chroot_dir, "/srv/jail");
        assert_eq!(back.uid, 1000);
        assert_eq!(back.rlimit_nofile, 1024);
        assert_eq!(back.rlimit_as, 1 << 30);
    }

    #[test]
    fn run_does_not_succeed_without_confinement() {
        let mut cfg = cfg();
        cfg.chroot_dir = "/nonexistent-vmm-jailer-run-test-do-not-create".into();
        let j = Jailer::new(cfg);
        #[cfg(target_os = "linux")]
        {
            let err = j.run().expect_err("missing chroot must fail closed");
            assert!(matches!(err, JailerError::Setup(_)));
        }
        #[cfg(not(target_os = "linux"))]
        {
            let err = j.run().expect_err("non-Linux jailer must be explicit");
            assert!(matches!(err, JailerError::Unimplemented(_)));
        }
    }

    #[test]
    fn config_with_zero_uid_round_trips() {
        // uid=0 (root) is a valid config (the jailer drops *to* a non-zero
        // uid, but the config can express "don't drop" as 0).
        let mut c = cfg();
        c.uid = 0;
        c.gid = 0;
        let s = serde_json::to_string(&c).unwrap();
        let back: JailerConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.uid, 0);
    }
}
