//! vCPU abstraction.
//!
//! On Linux+KVM (feature `kvm`) this wraps a `kvm_ioctls::Vcpu` and runs
//! `KVM_RUN` in a dedicated thread. Off-KVM, only the type exists so the rest
//! of the VMM compiles.

use crate::error::Result;
/// Per-vCPU identifier (0..vcpu_count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VcpuId(pub u8);

/// A vCPU handle. On KVM this owns the `kvm_ioctls::Vcpu`; the run loop lives
/// in `Vcpu::run`. Off-KVM the body is a no-op stub.
pub struct Vcpu {
    pub id: VcpuId,
    #[cfg(feature = "kvm")]
    pub kvm_cpu: kvm_ioctls::VcpuFd,
}

impl Vcpu {
    #[cfg(feature = "kvm")]
    pub fn new(id: VcpuId, vm: &kvm_ioctls::VmFd) -> Result<Self> {
        let kvm_cpu = vm
            .create_vcpu(id.0 as u64)
            .map_err(|e| crate::error::VmmError::Kvm(format!("create_vcpu({}): {e}", id.0)))?;
        Ok(Self { id, kvm_cpu })
    }

    #[cfg(not(feature = "kvm"))]
    pub fn new(id: VcpuId) -> Result<Self> {
        Ok(Self { id })
    }
}
