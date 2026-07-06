//! virtio-mmio transports and in-VMM virtio devices.
//!
//! virtio-mmio over the MMIO bus, with in-VMM backends for
//! block (file-backed) and net (tap-backed). vhost-user backends are a later
//! option.

pub mod blk;
pub mod blk_backend;
pub mod blk_transport;
pub mod net;
pub mod net_io_loop;
pub mod net_transport;
pub mod queue;
pub mod regs;
pub mod rng;
pub mod rng_transport;
pub mod transport;
pub mod vqueue;
pub mod vsock;
pub mod vsock_io_loop;

pub use blk::VirtioBlk;
pub use blk_transport::VirtioBlkMmio;
pub use net::VirtioNet;
pub use net_transport::VirtioNetMmio;
pub use rng::VirtioRng;
pub use rng_transport::VirtioRngMmio;
pub use transport::VirtioMmio;
pub use vqueue::{Descriptor, QueueConfig, UsedElem, VirtQueueProcessor};
pub use vsock::VirtioVsockMmio;
