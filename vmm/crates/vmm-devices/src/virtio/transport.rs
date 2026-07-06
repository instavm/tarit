//! virtio-mmio transport: decodes guest MMIO reads/writes into the register
//! layout defined in [`super::regs`], drives the underlying virtio device,
//! and raises an IRQ when the device needs the driver's attention.
//!
//! Spec: virtio v1.x, section "Virtio Transport Options / MMIO".
//! Full register decode + kick in M7; this is the typed state container.

use crate::bus::{MmioDevice, MmioReadResult, MmioWriteResult};
use crate::virtio::regs::{reg, MAGIC};
use std::sync::atomic::{AtomicU32, Ordering};

/// virtio device status bits (virtio v1.x, §4.2.3.1).
pub mod status {
    pub const ACKNOWLEDGE: u32 = 1 << 0;
    pub const DRIVER: u32 = 1 << 1;
    pub const FEATURES_OK: u32 = 1 << 2;
    pub const DRIVER_OK: u32 = 1 << 3;
    pub const DEVICE_NEEDS_RESET: u32 = 1 << 6;
    pub const FAILED: u32 = 1 << 7;
}

/// The virtio-mmio transport state for one device.
pub struct VirtioMmio {
    /// Guest IRQ this device raises on kick.
    pub irq: u32,
    /// The device's identity (read from MAGIC_VALUE/VERSION/DEVICE_ID).
    pub device_id: u32,
    pub vendor_id: u32,
    pub version: u32,
    /// Current device status bitfield.
    pub status: AtomicU32,
    /// Selected feature-bank for HOST_FEATURES / GUEST_FEATURES (32-bit chunks).
    pub host_features_sel: AtomicU32,
    pub guest_features_sel: AtomicU32,
    /// Selected queue index for QUEUE_SEL.
    pub queue_sel: AtomicU32,
}

impl VirtioMmio {
    pub fn new(irq: u32, device_id: u32, vendor_id: u32, version: u32) -> Self {
        Self {
            irq,
            device_id,
            vendor_id,
            version,
            status: AtomicU32::new(0),
            host_features_sel: AtomicU32::new(0),
            guest_features_sel: AtomicU32::new(0),
            queue_sel: AtomicU32::new(0),
        }
    }
}

impl MmioDevice for VirtioMmio {
    fn mmio_read(&self, off: u64, _len: u8) -> MmioReadResult {
        let val = match off {
            reg::MAGIC_VALUE => MAGIC,
            reg::VERSION => self.version,
            reg::DEVICE_ID => self.device_id,
            reg::VENDOR_ID => self.vendor_id,
            reg::QUEUE_NUM_MAX => 256, // v1 cap; real device may offer less
            reg::STATUS => self.status.load(Ordering::Relaxed),
            reg::INTERRUPT_STATUS => 0, // M7: track used-ring buffer event
            _ => 0,
        };
        Ok(val as u64)
    }

    fn mmio_write(&self, off: u64, val: u64, _len: u8) -> MmioWriteResult {
        let val = val as u32;
        match off {
            reg::STATUS => {
                self.status.store(val, Ordering::Relaxed);
            }
            reg::QUEUE_SEL => {
                self.queue_sel.store(val, Ordering::Relaxed);
            }
            reg::HOST_FEATURES_SEL => {
                self.host_features_sel.store(val, Ordering::Relaxed);
            }
            reg::GUEST_FEATURES_SEL => {
                self.guest_features_sel.store(val, Ordering::Relaxed);
            }
            reg::QUEUE_NOTIFY => {
                // M7: wake the device I/O thread for queue `val`.
                log::trace!("virtio-mmio QUEUE_NOTIFY queue={val}");
            }
            reg::INTERRUPT_ACK => {
                // M7: clear the interrupt bit.
            }
            _ => {
                // M7: queue desc/driver/device address registers, queue_num, etc.
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{MmioBus, MmioRange};

    #[test]
    fn magic_reads_back_as_virt() {
        let dev = VirtioMmio::new(5, /* block */ 2, 0, 2);
        assert_eq!(dev.mmio_read(reg::MAGIC_VALUE, 4).unwrap(), MAGIC as u64);
        let bytes = (dev.mmio_read(reg::MAGIC_VALUE, 4).unwrap() as u32).to_le_bytes();
        assert_eq!(&bytes, b"virt");
    }

    #[test]
    fn status_round_trips_through_register() {
        let dev = VirtioMmio::new(5, 2, 0, 2);
        dev.mmio_write(reg::STATUS, status::ACKNOWLEDGE as u64, 4)
            .unwrap();
        assert_eq!(
            dev.mmio_read(reg::STATUS, 4).unwrap(),
            status::ACKNOWLEDGE as u64
        );
    }

    #[test]
    fn bus_dispatches_to_virtio_device() {
        let mut bus = MmioBus::new();
        let dev = VirtioMmio::new(5, 2, 0, 2);
        bus.insert(MmioRange::new(0xd000_0000, 0x1000), Box::new(dev))
            .unwrap();
        // Reading MAGIC_VALUE at the device's base + 0 should give "virt".
        assert_eq!(
            bus.read(0xd000_0000 + reg::MAGIC_VALUE, 4).unwrap(),
            MAGIC as u64
        );
        // DEVICE_ID at base + 0x008 should give 2 (block).
        assert_eq!(bus.read(0xd000_0000 + reg::DEVICE_ID, 4).unwrap(), 2);
    }
}
