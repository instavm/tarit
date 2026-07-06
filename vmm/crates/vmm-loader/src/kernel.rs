//! Kernel image loading — thin wrapper over [`x86_64::load_and_setup_boot`].
//!
//! On x86_64 this loads the kernel (ELF `vmlinux` or `bzImage`) into guest
//! memory and writes the zero page, cmdline, and initramfs. Returns a
//! [`LoadedKernel`] with the entry-point addresses the VMM needs to set
//! vCPU registers before `KVM_RUN`.

#![cfg(target_arch = "x86_64")]

use crate::error::Result;
use crate::x86_64::{load_and_setup_boot, BootSetup};
use std::path::Path;
use vm_memory::GuestMemoryMmap;

/// Result of loading a kernel into guest memory.
#[derive(Debug, Clone)]
pub struct LoadedKernel {
    /// Guest physical address of the kernel entry point (where vCPU RIP goes).
    pub entry: u64,
    /// First GPA after the kernel image (initramfs placed here).
    pub kernel_end: u64,
    /// Address of the zero page (boot_params).
    pub zero_page_addr: u64,
    /// Address of the NUL-terminated cmdline.
    pub cmdline_addr: u64,
    /// Address of the initramfs, if loaded.
    pub initramfs_addr: Option<u64>,
    pub initramfs_size: u64,
}

impl From<BootSetup> for LoadedKernel {
    fn from(b: BootSetup) -> Self {
        LoadedKernel {
            entry: b.kernel_load.0,
            kernel_end: b.kernel_end.0,
            zero_page_addr: b.zero_page_addr.0,
            cmdline_addr: b.cmdline_addr.0,
            initramfs_addr: b.initramfs_addr.map(|a| a.0),
            initramfs_size: b.initramfs_size,
        }
    }
}

/// Load `kernel_path` (vmlinux ELF or bzImage) into `guest_memory`, write
/// the zero page + cmdline + initramfs, and return the entry-point info.
///
/// `mem_size_bytes` is the total guest RAM — needed to build the E820 map.
pub fn load<P: AsRef<Path>>(
    guest_memory: &GuestMemoryMmap,
    kernel_path: P,
    cmdline: &str,
    initramfs_path: Option<P>,
    mem_size_bytes: u64,
) -> Result<LoadedKernel> {
    let setup = load_and_setup_boot(
        guest_memory,
        kernel_path,
        cmdline,
        initramfs_path,
        mem_size_bytes,
    )?;
    Ok(setup.into())
}
