//! vmm-core: the VMM core — KVM VM/vCPU setup, CPUID/MSR templates, the vCPU
//! run loop, and MMIO exit dispatch.
//!
//! KVM is gated behind the `kvm` feature so that the non-KVM logic (config,
//! state machine, device wiring) compiles and unit-tests on macOS.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod clone;
pub mod config;
pub mod controller;
pub mod cpu_template;
pub mod error;
pub mod gc;
pub mod guest_channel;
pub mod oci;
pub mod pty_stream;
pub mod restore_semantics;
pub mod security;
pub mod state;
pub mod vcpu;
pub mod volume;

#[cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]
pub mod kvm;
#[cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]
pub mod live_snapshot;
#[cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]
pub mod vcpu_setup;
#[cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]
pub mod vcpu_thread;

#[cfg(all(feature = "boot", target_arch = "x86_64", target_os = "linux"))]
pub mod vsock_exec;
#[cfg(all(feature = "boot", target_arch = "x86_64", target_os = "linux"))]
pub mod vsock_pty;

#[cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]
pub use kvm::KvmVm;
#[cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]
pub use live_snapshot::{live_snapshot, LiveSnapshotConfig, LiveSnapshotResult};
#[cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]
pub use vcpu_setup::{
    set_lint, setup_ap_vcpu, setup_cpuid, setup_vcpu_for_bzimage_boot,
    setup_vcpu_for_bzimage_boot_full, setup_vcpu_for_kernel_boot, write_acpi_tables, write_gdt,
};
#[cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]
pub use vcpu_thread::VcpuThread;

pub use controller::VmmController;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
