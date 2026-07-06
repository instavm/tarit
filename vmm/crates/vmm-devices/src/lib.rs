//! vmm-devices: the device model — MMIO bus, virtio-mmio transport,
//! virtio-blk, virtio-net, serial, RTC.
//!
//! PRD §1: **MMIO-only transport, no PCI.** virtio-mmio + direct kernel boot
//! is the single biggest boot-time lever. We follow Firecracker's minimalist
//! device model: a tiny set of devices, each implementing a `Persist` trait
//! (snapshot/restore) from day one.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod bus;
pub mod io_loop;
pub mod persist;
pub mod rate_limit;
pub mod serial;
pub mod virtio;

pub use bus::{MmioBus, MmioRange};
pub use persist::Persist;
