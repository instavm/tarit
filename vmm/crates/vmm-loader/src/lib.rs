//! vmm-loader: kernel image parse + load + zero-page / boot params + cmdline.
//!
//! Direct kernel boot of an uncompressed `vmlinux` (or `bzImage`) +
//! minimal ext4 rootfs / initramfs. No firmware, no BIOS, no option ROMs —
//! this is where microVMs win.
//!
//! Built on `linux-loader 0.14`.
//!   - [`memmap`] — pure-Rust E820 map + x86 boot constants (runs on any host).
//!   - [`cmdline`] — kernel command-line builder (runs on any host).
//!   - [`x86_64`] — zero-page (boot_params) construction + kernel load
//!     (`target_arch = "x86_64"` only; cross-type-checked on other hosts).

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod cmdline;
pub mod error;
pub mod memmap;

#[cfg(target_arch = "x86_64")]
pub mod kernel;
#[cfg(target_arch = "x86_64")]
pub mod x86_64;

pub use cmdline::{build_cmdline, default_cmdline};
pub use memmap::{
    build_e820_map, E820Entry, E820_RAM, E820_RESERVED, EBDA_START, HIMEM_START,
    KERNEL_BOOT_FLAG_MAGIC, KERNEL_HDR_MAGIC, KERNEL_LOADER_OTHER, KERNEL_MIN_ALIGNMENT_BYTES,
    MMIO_GAP_END, MMIO_GAP_START, ZERO_PAGE_ADDR,
};

#[cfg(target_arch = "x86_64")]
pub use kernel::{load, LoadedKernel};
