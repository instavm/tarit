//! VM Generation ID support for snapshot restore and clone isolation.

use crate::error::{Result, VmmError};
use vmm_memory_backend::GuestMemory;
use vmm_sys_util::eventfd::EventFd;

/// The generation value lives in the E820-reserved BIOS area, outside memory
/// Linux may allocate. It is 16-byte sized and 8-byte aligned as required by
/// the VM Generation ID specification.
pub const VMGENID_GPA: u64 = 0x000e_6000;
pub const VMGENID_SIZE: usize = 16;

/// IRQ 3 is the sole unused legacy IOAPIC pin in Tarit's fixed layout. Keeping
/// it fixed avoids colliding with the variable virtio device range at GSI 5+.
pub const VMGENID_GSI: u32 = 3;

const DSDT_GPA: u64 = 0x000e_3000;
const DSDT_RESERVED_BYTES: usize = 0x1000;
const VMGENID_ACPI_HID: &[u8] = b"VMGENCTR";

const _: () = {
    assert!(VMGENID_GPA.is_multiple_of(8));
    assert!(VMGENID_GPA >= 0x000a_0000);
    assert!(VMGENID_GPA + VMGENID_SIZE as u64 <= 0x0010_0000);
    assert!(VMGENID_GPA >= DSDT_GPA + DSDT_RESERVED_BYTES as u64);
    assert!(VMGENID_GSI != 0);
    assert!(VMGENID_GSI != 1);
    assert!(VMGENID_GSI != 2);
    assert!(VMGENID_GSI != 4);
    assert!(VMGENID_GSI < 5);
};

pub struct VmGenId {
    interrupt_evt: EventFd,
}

impl VmGenId {
    pub fn new(mem: &GuestMemory) -> Result<Self> {
        let mut id = [0u8; VMGENID_SIZE];
        fill_random(&mut id)?;
        mem.write_phys(VMGENID_GPA, &id)
            .map_err(|error| VmmError::Memory(format!("write VM Generation ID: {error}")))?;
        let interrupt_evt = EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|error| VmmError::Kvm(format!("VM Generation ID EventFd: {error}")))?;
        Ok(Self { interrupt_evt })
    }

    pub fn eventfd(&self) -> &EventFd {
        &self.interrupt_evt
    }

    pub fn notify_after_restore(&self) -> Result<()> {
        self.interrupt_evt
            .write(1)
            .map_err(|error| VmmError::Kvm(format!("notify VM Generation ID change: {error}")))
    }

    pub fn into_eventfd(self) -> EventFd {
        self.interrupt_evt
    }
}

/// Old snapshots do not contain the VMGenID ACPI device. Resuming one would
/// execute duplicated kernel CRNG state without a notification, so reject it
/// instead of silently weakening clone isolation.
pub fn require_snapshot_support(mem: &GuestMemory) -> Result<()> {
    let mut dsdt = [0u8; DSDT_RESERVED_BYTES];
    mem.read_phys(DSDT_GPA, &mut dsdt)
        .map_err(|error| VmmError::Memory(format!("read snapshot DSDT: {error}")))?;
    if dsdt
        .windows(VMGENID_ACPI_HID.len())
        .any(|window| window == VMGENID_ACPI_HID)
    {
        return Ok(());
    }
    Err(VmmError::Snapshot(
        "snapshot predates VM Generation ID support and cannot be resumed safely".into(),
    ))
}

pub(crate) fn fill_random(output: &mut [u8]) -> Result<()> {
    let mut written = 0usize;
    while written < output.len() {
        // SAFETY: the pointer and remaining length describe the writable tail
        // of `output`; getrandom does not retain the pointer.
        let result = unsafe {
            libc::getrandom(
                output[written..].as_mut_ptr().cast(),
                output.len() - written,
                0,
            )
        };
        if result > 0 {
            written += result as usize;
            continue;
        }
        if result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        let error = if result == 0 {
            "getrandom returned zero bytes".into()
        } else {
            std::io::Error::last_os_error().to_string()
        };
        return Err(VmmError::Device(format!(
            "generate host random bytes: {error}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_values_are_not_reused() {
        let mut first = [0u8; VMGENID_SIZE];
        let mut second = [0u8; VMGENID_SIZE];
        fill_random(&mut first).unwrap();
        fill_random(&mut second).unwrap();
        assert_ne!(first, second);
    }
}
