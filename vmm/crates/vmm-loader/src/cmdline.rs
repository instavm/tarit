//! Kernel command-line builder.
//!
//! For example `console=ttyS0 reboot=k panic=1 pci=off i8042.noaux ...`
//! We enforce the kernel's hard limit of 4096 bytes for the x86 zero-page
//! cmdline and assemble a sane default for a minimal microVM.

use crate::error::{LoaderError, Result};

/// The kernel's cmdline hard limit on x86 (zero-page `cmdline_size`).
pub const CMDLINE_MAX: usize = 4096;

/// A minimal, fast-boot default cmdline for a microVM.
///
/// Tuned for cold-boot-to-exec latency (measured ~2× faster than the verbose
/// default on nested-virt c8i): the dominant cost is kernel console spam over
/// the emulated 16550 (one VM-exit per byte), so we suppress it with
/// `quiet loglevel=0` while KEEPING the UART enabled (we need `/dev/ttyS0` for
/// the guest exec agent, so we do not pass `8250.nr_uarts=0`).
///
/// - `console=ttyS0`        → serial console on the 16550 (agent I/O)
/// - `quiet loglevel=0`     → suppress kernel console output (biggest win)
/// - `reboot=k panic=-1`    → reboot immediately on panic (no 1s wait)
/// - `nomodule`             → no module loading (we build drivers in)
/// - `pci=off`              → MMIO-only, skip PCI enumeration
/// - `i8042.*`              → skip PS/2 keyboard/mouse probing
/// - `swiotlb=noforce`      → skip bounce-buffer setup
/// - `cryptomgr.notests`    → skip crypto self-tests
/// - `random.trust_cpu=on`  → trust RDRAND for instant CRNG init
/// - `tsc=reliable no_timer_check` → skip TSC/timer calibration (kvm-clock)
/// - `nowatchdog nokaslr`   → skip watchdog probe + KASLR
pub fn default_cmdline() -> String {
    "console=ttyS0 quiet loglevel=0 reboot=k panic=-1 nomodule pci=off \
     i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd swiotlb=noforce \
     cryptomgr.notests random.trust_cpu=on tsc=reliable no_timer_check \
     nowatchdog nokaslr"
        .to_string()
}

/// Build and validate a kernel cmdline from a list of `key=value` (or `key`)
/// fragments, returning the full string with a trailing NUL (the zero-page
/// expects a NUL-terminated string).
pub fn build_cmdline(parts: &[&str]) -> Result<String> {
    let mut s = parts.join(" ");
    if s.len() >= CMDLINE_MAX {
        return Err(LoaderError::CmdlineTooLong {
            len: s.len(),
            max: CMDLINE_MAX,
        });
    }
    s.push('\0');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        let d = default_cmdline();
        assert!(d.len() < CMDLINE_MAX);
        assert!(d.contains("console=ttyS0"));
        assert!(d.contains("pci=off"));
    }

    #[test]
    fn build_joins_and_nul_terminates() {
        let c = build_cmdline(&["console=ttyS0", "panic=1"]).unwrap();
        assert_eq!(c, "console=ttyS0 panic=1\0");
    }

    #[test]
    fn rejects_too_long() {
        let huge: Vec<&str> = (0..5000).map(|_| "x").collect();
        assert!(matches!(
            build_cmdline(&huge),
            Err(LoaderError::CmdlineTooLong { .. })
        ));
    }

    #[test]
    fn empty_parts_produce_just_nul() {
        let c = build_cmdline(&[]).unwrap();
        assert_eq!(c, "\0");
    }
}
