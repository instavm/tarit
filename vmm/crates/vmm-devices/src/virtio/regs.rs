//! virtio-mmio transport register layout (virtio v1.x, "Virtio over MMIO").
//!
//! Offsets from the virtio v1.x spec, §4.2 (MMIO v2 layout).
//! Each register is 32 bits; the layout is fixed by the spec.

use serde::{Deserialize, Serialize};

/// virtio-mmio register offsets. Each register is 32 bits.
/// Offsets from the Linux kernel uapi: include/uapi/linux/virtio_mmio.h
pub mod reg {
    /// `0x000` — magic value, must read as 0x74726976 ("virt").
    pub const MAGIC_VALUE: u64 = 0x000;
    /// `0x004` — device version (1 = legacy, 2 = modern).
    pub const VERSION: u64 = 0x004;
    /// `0x008` — virtio device ID (0 = invalid/reserved; 1 = net, 2 = block).
    pub const DEVICE_ID: u64 = 0x008;
    /// `0x00c` — vendor ID.
    pub const VENDOR_ID: u64 = 0x00c;
    /// `0x010` — bitfield of features the device supports (low 32 bits).
    pub const HOST_FEATURES: u64 = 0x010;
    /// `0x014` — W: driver selects which 32-bit chunk of host features to read.
    pub const HOST_FEATURES_SEL: u64 = 0x014;
    /// `0x020` — W: driver writes its selected features.
    pub const GUEST_FEATURES: u64 = 0x020;
    /// `0x024` — W: driver selects which 32-bit chunk to write.
    pub const GUEST_FEATURES_SEL: u64 = 0x024;
    /// `0x030` — R/W: the queue index the driver is configuring.
    pub const QUEUE_SEL: u64 = 0x030;
    /// `0x034` — R: max size of the selected queue.
    pub const QUEUE_NUM_MAX: u64 = 0x034;
    /// `0x038` — W: the size the driver chose (power of two, <= QUEUE_NUM_MAX).
    pub const QUEUE_NUM: u64 = 0x038;
    /// `0x044` — R/W: queue ready flag (v2). 0 = not ready, 1 = ready.
    pub const QUEUE_READY: u64 = 0x044;
    /// `0x050` — W: driver kicks the device (notify on a queue).
    pub const QUEUE_NOTIFY: u64 = 0x050;
    /// `0x060` — R: interrupt status.
    pub const INTERRUPT_STATUS: u64 = 0x060;
    /// `0x064` — W: interrupt ack.
    pub const INTERRUPT_ACK: u64 = 0x064;
    /// `0x070` — R/W: device status bitfield.
    pub const STATUS: u64 = 0x070;
    /// `0x080` — R/W: low 32 bits of the descriptor table GPA (v2).
    pub const QUEUE_DESC_LOW: u64 = 0x080;
    /// `0x084` — R/W: high 32 bits of the descriptor table GPA (v2).
    pub const QUEUE_DESC_HIGH: u64 = 0x084;
    /// `0x090` — R/W: low 32 bits of the available ring GPA (v2).
    pub const QUEUE_DRIVER_LOW: u64 = 0x090;
    /// `0x094` — R/W: high 32 bits of the available ring GPA (v2).
    pub const QUEUE_DRIVER_HIGH: u64 = 0x094;
    /// `0x0a0` — R/W: low 32 bits of the used ring GPA (v2).
    pub const QUEUE_DEVICE_LOW: u64 = 0x0a0;
    /// `0x0a4` — R/W: high 32 bits of the used ring GPA (v2).
    pub const QUEUE_DEVICE_HIGH: u64 = 0x0a4;
    /// `0x0fc` — R: config generation (changes on config updates).
    pub const CONFIG_GENERATION: u64 = 0x0fc;
    /// `0x100` — R/W: device-specific configuration space.
    pub const CONFIG: u64 = 0x100;
}

/// The magic value a virtio-mmio device's first register must read: "virt".
pub const MAGIC: u32 = 0x7472_6976;

/// A device's static identity (read from the first few registers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtioMmioId {
    pub device_id: u32,
    pub vendor_id: u32,
    pub version: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_reads_as_virt() {
        let bytes = MAGIC.to_le_bytes();
        assert_eq!(&bytes, b"virt");
    }

    #[test]
    fn register_offsets_are_4_byte_aligned() {
        for &off in &[
            reg::MAGIC_VALUE,
            reg::VERSION,
            reg::DEVICE_ID,
            reg::VENDOR_ID,
            reg::HOST_FEATURES,
            reg::QUEUE_SEL,
            reg::QUEUE_NUM_MAX,
            reg::QUEUE_NUM,
            reg::QUEUE_READY,
            reg::QUEUE_DESC_LOW,
            reg::QUEUE_DESC_HIGH,
            reg::QUEUE_DRIVER_LOW,
            reg::QUEUE_DRIVER_HIGH,
            reg::QUEUE_DEVICE_LOW,
            reg::QUEUE_DEVICE_HIGH,
            reg::QUEUE_NOTIFY,
            reg::STATUS,
            reg::INTERRUPT_STATUS,
            reg::INTERRUPT_ACK,
        ] {
            assert_eq!(off % 4, 0, "offset 0x{off:x} not 4-byte aligned");
        }
    }

    #[test]
    fn queue_ready_is_at_0x44_per_v2_spec() {
        assert_eq!(reg::QUEUE_READY, 0x044);
    }

    #[test]
    fn no_offset_collisions() {
        let offsets = [
            reg::MAGIC_VALUE,
            reg::VERSION,
            reg::DEVICE_ID,
            reg::VENDOR_ID,
            reg::HOST_FEATURES,
            reg::HOST_FEATURES_SEL,
            reg::GUEST_FEATURES,
            reg::GUEST_FEATURES_SEL,
            reg::QUEUE_SEL,
            reg::QUEUE_NUM_MAX,
            reg::QUEUE_NUM,
            reg::QUEUE_READY,
            reg::QUEUE_DESC_LOW,
            reg::QUEUE_DESC_HIGH,
            reg::QUEUE_DRIVER_LOW,
            reg::QUEUE_DRIVER_HIGH,
            reg::QUEUE_DEVICE_LOW,
            reg::QUEUE_DEVICE_HIGH,
            reg::QUEUE_NOTIFY,
            reg::STATUS,
            reg::INTERRUPT_STATUS,
            reg::INTERRUPT_ACK,
        ];
        let set: std::collections::HashSet<u64> = offsets.iter().copied().collect();
        assert_eq!(set.len(), offsets.len(), "register offset collision");
    }
}
