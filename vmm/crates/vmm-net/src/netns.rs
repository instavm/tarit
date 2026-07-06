//! Per-VM network namespace orchestration (PRD §8).
//!
//! PRD §8: "each microVM gets a virtio-net device backed by a host tap
//! interface, and that tap lives inside a dedicated network namespace per
//! VM."
//!
//! Linux-only (netns requires `unshare(CLONE_NEWNET)` / `setns`). vmm-net does
//! not currently create or enter host network namespaces; that setup is the
//! orchestrator's responsibility. These APIs return explicit errors rather than
//! pretending isolation happened.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NetNsError {
    #[error("netns not supported on this platform")]
    Unsupported,
    #[error("netns orchestration is not implemented in vmm-net; host netns setup is the orchestrator's job")]
    Unimplemented,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("syscall: {0}")]
    Syscall(String),
}

/// A handle to a per-VM network namespace. Host namespace creation/entry is
/// intentionally left to the orchestrator until vmm-net wires the real syscalls.
#[derive(Debug)]
pub struct NetNs {
    pub name: String,
    /// The netns fd (`/var/run/netns/<name>` bind-mount). Used in M8's
    /// real tap backend.
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    fd: Option<std::os::fd::RawFd>,
}

#[cfg(target_os = "linux")]
impl NetNs {
    /// Creating netns from vmm-net is not implemented. The orchestrator must
    /// set up the host namespace and launch/enter the VMM there.
    pub fn create(_name: &str) -> Result<Self, NetNsError> {
        Err(NetNsError::Unimplemented)
    }

    /// Entering netns from vmm-net is not implemented. The orchestrator must
    /// enter the namespace before applying tap/nft setup.
    pub fn enter(&self) -> Result<(), NetNsError> {
        Err(NetNsError::Unimplemented)
    }
}

#[cfg(not(target_os = "linux"))]
impl NetNs {
    pub fn create(_name: &str) -> Result<Self, NetNsError> {
        Err(NetNsError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn create_returns_unimplemented_on_linux() {
        let err = NetNs::create("vmm0").unwrap_err();
        assert!(matches!(err, NetNsError::Unimplemented));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn enter_returns_unimplemented_on_linux() {
        let ns = NetNs {
            name: "vmm0".into(),
            fd: None,
        };
        let err = ns.enter().unwrap_err();
        assert!(matches!(err, NetNsError::Unimplemented));
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn create_returns_unsupported_off_linux() {
        let err = NetNs::create("vmm0").unwrap_err();
        assert!(matches!(err, NetNsError::Unsupported));
    }
}
