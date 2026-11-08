//! Live virtio-mem resize for a running VM.
//!
//! The controller resolves the VM's [`VirtioMemMmio`] device and delegates
//! here; this module holds no KVM state, so the whole request/wait contract is
//! unit-testable against a simulated guest driver.
//!
//! Sizes on the wire are MiB and *exclude boot RAM*, matching Firecracker's
//! `requested_size_mib`: guest-visible RAM is `boot + plugged`, and the host
//! reservation is `boot + total`.

use crate::error::{Result, VmmError};
use std::time::{Duration, Instant};
use tarit_proto::{MemoryHotplugStatus, MAX_MEMORY_HOTPLUG_WAIT_MS};
use vmm_devices::virtio::mem::VirtioMemMmio;

const MIB: u64 = 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Current device sizes in MiB.
pub fn status(device: &VirtioMemMmio) -> MemoryHotplugStatus {
    let status = device.status();
    MemoryHotplugStatus {
        total_size_mib: status.region_size / MIB,
        block_size_mib: status.block_size / MIB,
        plugged_size_mib: status.plugged_size / MIB,
        requested_size_mib: status.requested_size / MIB,
        boot_memory_mib: status.boot_size / MIB,
    }
}

/// Set the requested hotplug size (MiB, excluding boot RAM) and optionally
/// wait until the guest driver's plugged size matches it.
///
/// Invalid sizes and waits are rejected before any device state changes. A
/// wait that times out returns an error but leaves the request applied: the
/// guest keeps converging and [`status`] reports progress.
pub fn set_requested(
    device: &VirtioMemMmio,
    requested_size_mib: u64,
    wait: Option<Duration>,
) -> Result<MemoryHotplugStatus> {
    if let Some(wait) = wait {
        if wait > Duration::from_millis(MAX_MEMORY_HOTPLUG_WAIT_MS) {
            return Err(VmmError::InvalidConfig(format!(
                "wait_timeout_ms must be at most {MAX_MEMORY_HOTPLUG_WAIT_MS}"
            )));
        }
    }
    let bytes = requested_size_mib
        .checked_mul(MIB)
        .ok_or_else(|| VmmError::InvalidConfig("requested hotplug size overflows bytes".into()))?;
    device
        .set_requested_size(bytes)
        .map_err(VmmError::InvalidConfig)?;
    match wait {
        None => Ok(status(device)),
        Some(wait) => wait_until_converged(device, wait),
    }
}

/// Poll until plugged == requested or `timeout` passes.
pub fn wait_until_converged(
    device: &VirtioMemMmio,
    timeout: Duration,
) -> Result<MemoryHotplugStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        let current = status(device);
        if current.plugged_size_mib == current.requested_size_mib {
            return Ok(current);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(VmmError::Device(format!(
                "guest did not reach the requested hotplug size within {} ms \
                 (plugged {} MiB, requested {} MiB); the request stays applied",
                timeout.as_millis(),
                current.plugged_size_mib,
                current.requested_size_mib
            )));
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline - now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use vmm_devices::bus::MmioDevice;
    use vmm_devices::virtio::mem::{BLOCK_SIZE, REGION_START};
    use vmm_devices::virtio::regs::reg;
    use vmm_memory_backend::GuestMemory;

    const BLOCK_MIB: u64 = BLOCK_SIZE / MIB;
    // Virtqueue and request buffers live in boot RAM, like a real driver's.
    const DESC: u64 = 0x10_0000;
    const AVAIL: u64 = DESC + 0x1000;
    const USED: u64 = DESC + 0x2000;
    const REQ: u64 = DESC + 0x3000;
    const RESP: u64 = DESC + 0x4000;
    const QUEUE_SIZE: u16 = 8;

    /// Minimal stand-in for Linux's virtio-mem driver: one request at a time
    /// through a real split virtqueue in guest RAM, converging the plugged
    /// size towards the config-space requested size. `pinned` blocks refuse
    /// to unplug, like memory holding unmovable guest allocations.
    struct FakeDriver {
        device: Arc<VirtioMemMmio>,
        memory: GuestMemory,
        avail_idx: u16,
        plugged: BTreeSet<u64>,
        pinned: BTreeSet<u64>,
    }

    impl FakeDriver {
        fn probe(device: Arc<VirtioMemMmio>, memory: GuestMemory) -> Self {
            for (register, value) in [
                (reg::STATUS, 1u64 | 2 | 8),
                (reg::QUEUE_SEL, 0),
                (reg::QUEUE_NUM, u64::from(QUEUE_SIZE)),
                (reg::QUEUE_DESC_LOW, DESC),
                (reg::QUEUE_DESC_HIGH, 0),
                (reg::QUEUE_DRIVER_LOW, AVAIL),
                (reg::QUEUE_DRIVER_HIGH, 0),
                (reg::QUEUE_DEVICE_LOW, USED),
                (reg::QUEUE_DEVICE_HIGH, 0),
                (reg::QUEUE_READY, 1),
                (reg::STATUS, 1 | 2 | 8 | 4),
            ] {
                device.mmio_write(register, value, 4).unwrap();
            }
            let mut descriptors = [0u8; 32];
            descriptors[0..8].copy_from_slice(&REQ.to_le_bytes());
            descriptors[8..12].copy_from_slice(&24u32.to_le_bytes());
            descriptors[12..14].copy_from_slice(&1u16.to_le_bytes()); // NEXT
            descriptors[14..16].copy_from_slice(&1u16.to_le_bytes());
            descriptors[16..24].copy_from_slice(&RESP.to_le_bytes());
            descriptors[24..28].copy_from_slice(&10u32.to_le_bytes());
            descriptors[28..30].copy_from_slice(&2u16.to_le_bytes()); // WRITE
            memory.write_phys(DESC, &descriptors).unwrap();
            let mut driver = Self {
                device,
                memory,
                avail_idx: 0,
                plugged: BTreeSet::new(),
                pinned: BTreeSet::new(),
            };
            // Linux probe: start clean if the device reports plugged memory.
            if driver.config(40) != 0 {
                assert_eq!(driver.send(2, 0, 0), 0);
            }
            driver
        }

        fn config(&self, offset: u64) -> u64 {
            self.device.mmio_read(reg::CONFIG + offset, 8).unwrap()
        }

        fn send(&mut self, kind: u16, block: u64, count: u16) -> u16 {
            let mut request = [0u8; 24];
            request[0..2].copy_from_slice(&kind.to_le_bytes());
            request[8..16].copy_from_slice(&(REGION_START + block * BLOCK_SIZE).to_le_bytes());
            request[16..18].copy_from_slice(&count.to_le_bytes());
            self.memory.write_phys(REQ, &request).unwrap();
            let slot = u64::from(self.avail_idx % QUEUE_SIZE);
            self.memory
                .write_phys(AVAIL + 4 + 2 * slot, &0u16.to_le_bytes())
                .unwrap();
            self.avail_idx = self.avail_idx.wrapping_add(1);
            self.memory
                .write_phys(AVAIL + 2, &self.avail_idx.to_le_bytes())
                .unwrap();
            self.device.mmio_write(reg::QUEUE_NOTIFY, 0, 4).unwrap();
            let mut reply = [0u8; 2];
            self.memory.read_phys(RESP, &mut reply).unwrap();
            u16::from_le_bytes(reply)
        }

        /// One step towards the requested size; false when idle.
        fn step(&mut self) -> bool {
            let blocks = self.config(24) / BLOCK_SIZE;
            let requested = self.config(48) / BLOCK_SIZE;
            let plugged = self.plugged.len() as u64;
            if plugged < requested {
                let free = (0..blocks).find(|b| !self.plugged.contains(b)).unwrap();
                if self.send(0, free, 1) == 0 {
                    self.plugged.insert(free);
                    return true;
                }
            } else if plugged > requested {
                let victim = self
                    .plugged
                    .iter()
                    .rev()
                    .copied()
                    .find(|b| !self.pinned.contains(b));
                if let Some(victim) = victim {
                    if self.send(1, victim, 1) == 0 {
                        self.plugged.remove(&victim);
                        return true;
                    }
                }
            }
            false
        }
    }

    fn spawn(mut driver: FakeDriver) -> (Arc<AtomicBool>, std::thread::JoinHandle<FakeDriver>) {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::Acquire) {
                if !driver.step() {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            driver
        });
        (stop, handle)
    }

    /// 1 GiB boot (8 blocks) + a 1 GiB aperture (8 blocks).
    fn vm() -> (Arc<VirtioMemMmio>, GuestMemory) {
        let memory = GuestMemory::new_hotplug(16 * BLOCK_SIZE, 8 * BLOCK_SIZE).unwrap();
        let device = Arc::new(VirtioMemMmio::new(10, memory.clone()).unwrap());
        (device, memory)
    }

    #[test]
    fn status_reports_mib_excluding_boot_ram() {
        let (device, _memory) = vm();
        assert_eq!(
            status(&device),
            MemoryHotplugStatus {
                total_size_mib: 1024,
                block_size_mib: BLOCK_MIB,
                plugged_size_mib: 0,
                requested_size_mib: 0,
                boot_memory_mib: 1024,
            }
        );
    }

    #[test]
    fn live_grow_and_shrink_to_zero_with_wait() {
        let (device, memory) = vm();
        let (stop, driver) = spawn(FakeDriver::probe(device.clone(), memory.clone()));
        let wait = Some(Duration::from_secs(10));

        let grown = set_requested(&device, 4 * BLOCK_MIB, wait).unwrap();
        assert_eq!(grown.plugged_size_mib, 4 * BLOCK_MIB);
        assert_eq!(grown.requested_size_mib, 4 * BLOCK_MIB);
        memory.write_phys(REGION_START, &[0xa5; 32]).unwrap();

        let full = set_requested(&device, 8 * BLOCK_MIB, wait).unwrap();
        assert_eq!(full.plugged_size_mib, 1024);

        let shrunk = set_requested(&device, BLOCK_MIB, wait).unwrap();
        assert_eq!(shrunk.plugged_size_mib, BLOCK_MIB);
        memory.drain_host_dirty();
        let empty = set_requested(&device, 0, wait).unwrap();
        assert_eq!(empty.plugged_size_mib, 0);
        assert_eq!(empty.requested_size_mib, 0);
        // The last unplug discarded block 0: its bytes are gone and its pages
        // are host-dirty for the next diff snapshot.
        let mut bytes = [0xff; 32];
        memory.read_phys(REGION_START, &mut bytes).unwrap();
        assert_eq!(bytes, [0; 32]);
        assert!(memory.drain_host_dirty().contains(memory.low_size_bytes));

        stop.store(true, Ordering::Release);
        let driver = driver.join().unwrap();
        assert!(driver.plugged.is_empty());
    }

    #[test]
    fn set_without_wait_returns_before_the_guest_converges() {
        let (device, _memory) = vm();
        let result = set_requested(&device, 2 * BLOCK_MIB, None).unwrap();
        assert_eq!(result.requested_size_mib, 2 * BLOCK_MIB);
        assert_eq!(result.plugged_size_mib, 0);
        assert!(device.has_pending_interrupt());
    }

    #[test]
    fn wait_timeout_reports_progress_and_keeps_the_request() {
        let (device, memory) = vm();
        let mut driver = FakeDriver::probe(device.clone(), memory.clone());
        set_requested(&device, 2 * BLOCK_MIB, None).unwrap();
        while driver.step() {}
        driver.pinned.insert(0);
        let (stop, handle) = spawn(driver);

        let error = set_requested(&device, 0, Some(Duration::from_millis(200)))
            .unwrap_err()
            .to_string();
        assert!(error.contains("plugged 128 MiB"), "{error}");
        assert!(error.contains("requested 0 MiB"), "{error}");
        let after = status(&device);
        assert_eq!(after.requested_size_mib, 0);
        assert_eq!(after.plugged_size_mib, BLOCK_MIB);
        // A zero wait is a single convergence check.
        assert!(wait_until_converged(&device, Duration::ZERO).is_err());

        stop.store(true, Ordering::Release);
        handle.join().unwrap();
    }

    #[test]
    fn invalid_requests_are_rejected_without_side_effects() {
        let (device, _memory) = vm();
        set_requested(&device, BLOCK_MIB, None).unwrap();
        let before = device.status();
        for (requested, wait) in [
            (8 * BLOCK_MIB + BLOCK_MIB, None),
            (100, None),
            (BLOCK_MIB + 1, None),
            (u64::MAX, None),
            (
                0,
                Some(Duration::from_millis(MAX_MEMORY_HOTPLUG_WAIT_MS + 1)),
            ),
        ] {
            let error = set_requested(&device, requested, wait).unwrap_err();
            assert!(matches!(error, VmmError::InvalidConfig(_)), "{error}");
            assert_eq!(device.status(), before);
        }
    }

    #[test]
    fn a_re_probing_driver_unplugs_all_and_re_plugs_the_request() {
        let (device, memory) = vm();
        let mut first = FakeDriver::probe(device.clone(), memory.clone());
        set_requested(&device, 3 * BLOCK_MIB, None).unwrap();
        while first.step() {}
        assert_eq!(status(&device).plugged_size_mib, 3 * BLOCK_MIB);
        memory.write_phys(0x20_0000, &[0x77; 8]).unwrap();

        // Guest reboot: device reset, then a fresh driver probe.
        device.mmio_write(reg::STATUS, 0, 4).unwrap();
        let mut second = FakeDriver::probe(device.clone(), memory.clone());
        assert_eq!(status(&device).plugged_size_mib, 0);
        while second.step() {}
        assert_eq!(status(&device).plugged_size_mib, 3 * BLOCK_MIB);
        let mut boot = [0; 8];
        memory.read_phys(0x20_0000, &mut boot).unwrap();
        assert_eq!(boot, [0x77; 8], "boot RAM untouched by UNPLUG_ALL");
    }
}
