//! MMIO bus — the single transport for all devices (no PCI).
//!
//! Each device gets a 4 KiB MMIO range at a guest-physical address. vCPU MMIO
//! exits are dispatched here: one flat bus, no PCI hierarchy.

use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BusError {
    #[error("mmio range 0x{base:x}..0x{end:x} overlaps an existing device")]
    Overlap { base: u64, end: u64 },
    #[error("no device mapped at 0x{0:x}")]
    Unmapped(u64),
}

/// An MMIO access failure (unmapped address, bad alignment, device error).
#[derive(Debug, Error)]
pub enum MmioError {
    #[error("no device mapped at 0x{0:x}")]
    Unmapped(u64),
    #[error("device rejected the access")]
    Device,
}

pub type MmioReadResult = std::result::Result<u64, MmioError>;
pub type MmioWriteResult = std::result::Result<(), MmioError>;

/// A 4 KiB MMIO range assigned to a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmioRange {
    pub base: u64,
    pub size: u64,
}

impl MmioRange {
    pub fn new(base: u64, size: u64) -> Self {
        Self { base, size }
    }
    pub fn end(&self) -> u64 {
        self.base + self.size
    }
    pub fn contains(&self, addr: u64) -> bool {
        (self.base..self.end()).contains(&addr)
    }
}

/// A device that can be addressed over MMIO.
pub trait MmioDevice: Send + Sync {
    /// Read `len` bytes at `offset` within this device's MMIO range.
    fn mmio_read(&self, offset: u64, len: u8) -> MmioReadResult;
    /// Write `val` of `len` bytes at `offset` within this device's MMIO range.
    fn mmio_write(&self, offset: u64, val: u64, len: u8) -> MmioWriteResult;
}

/// Lets the diagnostic test hold an `Arc<VirtioBlkMmio>` while the bus also
/// owns one for dispatch. Without this, `Box<dyn MmioDevice>` would consume
/// the device and the test couldn't read its counters post-boot.
impl<T: MmioDevice + ?Sized> MmioDevice for std::sync::Arc<T> {
    fn mmio_read(&self, offset: u64, len: u8) -> MmioReadResult {
        T::mmio_read(self.as_ref(), offset, len)
    }
    fn mmio_write(&self, offset: u64, val: u64, len: u8) -> MmioWriteResult {
        T::mmio_write(self.as_ref(), offset, val, len)
    }
}

/// The MMIO bus: dispatches vCPU MMIO exits to the right device.
pub struct MmioBus {
    /// base address → (range, device)
    devices: BTreeMap<u64, (MmioRange, Box<dyn MmioDevice>)>,
}

impl MmioBus {
    pub fn new() -> Self {
        Self {
            devices: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, range: MmioRange, dev: Box<dyn MmioDevice>) -> Result<(), BusError> {
        // Check for overlap against every existing range.
        for (&_base, (r, _)) in &self.devices {
            if range.base < r.end() && r.base < range.end() {
                return Err(BusError::Overlap {
                    base: range.base,
                    end: range.end(),
                });
            }
        }
        self.devices.insert(range.base, (range, dev));
        Ok(())
    }

    pub fn read(&self, addr: u64, len: u8) -> MmioReadResult {
        let (range, dev) = self.device_at(addr).ok_or(MmioError::Unmapped(addr))?;
        let offset = addr - range.base;
        dev.mmio_read(offset, len)
    }

    pub fn write(&self, addr: u64, val: u64, len: u8) -> MmioWriteResult {
        let (range, dev) = self.device_at(addr).ok_or(MmioError::Unmapped(addr))?;
        let offset = addr - range.base;
        dev.mmio_write(offset, val, len)
    }

    /// Find the device whose range contains `addr`.
    fn device_at(&self, addr: u64) -> Option<(&MmioRange, &dyn MmioDevice)> {
        self.devices
            .range(..=addr)
            .next_back()
            .filter(|(_, (r, _))| r.contains(addr))
            .map(|(_, (r, d))| (r, d.as_ref()))
    }
}

impl Default for MmioBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoDev;
    impl MmioDevice for EchoDev {
        fn mmio_read(&self, off: u64, _len: u8) -> MmioReadResult {
            Ok(off)
        }
        fn mmio_write(&self, _off: u64, _val: u64, _len: u8) -> MmioWriteResult {
            Ok(())
        }
    }

    #[test]
    fn insert_and_read() {
        let mut bus = MmioBus::new();
        bus.insert(MmioRange::new(0xd000_0000, 0x1000), Box::new(EchoDev))
            .unwrap();
        assert_eq!(bus.read(0xd000_0000 + 5, 8).unwrap(), 5);
        assert!(bus.read(0xc000_0000, 8).is_err());
    }

    #[test]
    fn overlap_rejected() {
        let mut bus = MmioBus::new();
        bus.insert(MmioRange::new(0x1000, 0x1000), Box::new(EchoDev))
            .unwrap();
        assert!(bus
            .insert(MmioRange::new(0x1800, 0x1000), Box::new(EchoDev))
            .is_err());
    }
}
