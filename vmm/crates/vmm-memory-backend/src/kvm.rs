//! KVM memory-region registration — `KVM_SET_USER_MEMORY_REGION`.
//!
//! PRD §6: "Guest RAM is an mmap'd region owned by the VMM and registered
//! with KVM." This module wires each `GuestMemory` region into the VM so
//! KVM can map guest physical addresses to host userspace pages.
//!
//! Only compiled on Linux (KVM is Linux-only). Behind the `kvm` feature so
//! it's only pulled in when the VMM actually needs KVM; the rest of the
//! memory backend compiles and tests without it.

#![cfg(all(feature = "kvm", target_os = "linux"))]

use crate::backend::{GuestMemory, MemoryError};
use kvm_bindings::{kvm_userspace_memory_region, KVM_MEM_LOG_DIRTY_PAGES};
use kvm_ioctls::VmFd;
use vm_memory::{Address, GuestMemoryBackend as _, GuestMemoryRegion};

/// A KVM slot ID (0..KVM_USERSPACE_LOCAL_API_MEM_SLOTS, typically 0..127).
pub type SlotId = u32;

impl GuestMemory {
    /// Register every guest memory region with the VM via
    /// `KVM_SET_USER_MEMORY_REGION`.
    ///
    /// `flags = 0` for plain registration. Pass `KVM_MEM_LOG_DIRTY_PAGES`
    /// (via [`Self::register_with_dirty_logging`]) to enable the dirty
    /// bitmap for snapshot/migration.
    pub fn register(&self, vm: &VmFd, flags: u32) -> Result<Vec<SlotId>, MemoryError> {
        let mut slots = Vec::new();
        for (slot, region) in (0_u32..).zip(self.inner.iter()) {
            let guest_phys = region.start_addr().raw_value();
            let host_ptr = region.as_ptr() as u64;
            let size = region.len();

            let region = kvm_userspace_memory_region {
                slot,
                flags,
                guest_phys_addr: guest_phys,
                memory_size: size,
                userspace_addr: host_ptr,
            };
            // SAFETY: `set_user_memory_region` is a plain ioctl wrapper; the
            // safety contract is that `host_ptr` points to a valid mapping
            // owned by the caller for `size` bytes. Our `GuestMemoryMmap`
            // owns exactly that mapping for the region's lifetime.
            unsafe {
                vm.set_user_memory_region(region).map_err(|e| {
                    MemoryError::Region(format!("KVM_SET_USER_MEMORY_REGION slot {slot}: {e}"))
                })?;
            }
            slots.push(slot);
        }
        Ok(slots)
    }

    /// Register memory regions with dirty-page logging enabled (PRD §6, §9c).
    pub fn register_with_dirty_logging(&self, vm: &VmFd) -> Result<Vec<SlotId>, MemoryError> {
        self.register(vm, KVM_MEM_LOG_DIRTY_PAGES)
    }
}
