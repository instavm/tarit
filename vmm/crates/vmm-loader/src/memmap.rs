//! Guest physical memory map — the E820 layout for x86_64 microVMs.
//!
//! This module is pure arithmetic with no x86-only types, so it (and its
//! tests) compile and run on any host. The actual `boot_params` struct
//! wiring lives in [`crate::x86_64`] and is `target_arch = "x86_64"`-gated.
//!
//! Layout (the standard x86_64 microVM memory map, as in rust-vmm's
//! vmm-reference):
//!
//! | GPA range | Type | Purpose |
//! |---|---|---|
//! | `0x0`..`0xA_0000` | RAM | low memory (640 KiB) |
//! | `0xA_0000`..`0x10_0000` | reserved | legacy VGA + BIOS data (384 KiB) |
//! | `0x10_0000`..`0xd000_0000` | RAM | high memory before the MMIO gap |
//! | `0xd000_0000`..`0x1_0000_0000` | reserved | virtio MMIO, PCI config, and KVM TSS space |
//! | `0x1_0000_0000`..`mem_end` | RAM | future split-region high memory |

// --- x86_64 boot / E820 constants (from kernel Documentation/x86/boot.txt) --

/// `boot_flag` magic — 0xaa55.
pub const KERNEL_BOOT_FLAG_MAGIC: u16 = 0xaa55;
/// `header` magic — `HdrS` (0x5372_6448).
pub const KERNEL_HDR_MAGIC: u32 = 0x5372_6448;
/// `type_of_loader` for an unregistered bootloader.
pub const KERNEL_LOADER_OTHER: u8 = 0xff;
/// `kernel_alignment` for a relocatable kernel.
pub const KERNEL_MIN_ALIGNMENT_BYTES: u32 = 0x0100_0000;
/// Start of the EBDA (Extended Bios Data Area).
pub const EBDA_START: u64 = 0x0009_fc00;
/// E820 memory type: usable RAM.
pub const E820_RAM: u32 = 1;
/// E820 memory type: reserved.
pub const E820_RESERVED: u32 = 2;

// --- Guest physical address layout (matches rust-vmm's vmm-reference) ---

/// The zero page (boot_params) lives at this GPA. For bzImage, the zero page
/// IS the setup code's header at 0x10000 — the VMM patches the header fields
/// directly in the loaded setup code rather than writing a separate
/// `boot_params` struct (which would clobber the setup code's data).
pub const ZERO_PAGE_ADDR: u64 = 0x0001_0000;
/// High-memory start — the kernel is loaded just above this.
pub const HIMEM_START: u64 = 0x0010_0000; // 1 MiB
/// Start of the MMIO gap.
pub const MMIO_GAP_START: u64 = 0xd000_0000; // 3.25 GiB
/// End of the MMIO gap.
pub const MMIO_GAP_END: u64 = 0x1_0000_0000; // 4 GiB

// --- E820 map construction -------------------------------------------------

/// An E820 map entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct E820Entry {
    pub addr: u64,
    pub size: u64,
    pub mem_type: u32,
}

/// Build the E820 map for a guest with `mem_size_bytes` of packed RAM.
///
/// See the module docs for the layout. For small VMs whose RAM ends before
/// the MMIO gap, only the first three entries are emitted.
pub fn build_e820_map(mem_size_bytes: u64) -> Vec<E820Entry> {
    let mut entries = Vec::with_capacity(5);

    // 1. Low memory 0..0xA_0000 (640 KiB).
    entries.push(E820Entry {
        addr: 0,
        size: 0xA_0000,
        mem_type: E820_RAM,
    });

    // 2. Reserved 0xA_0000..0x10_0000 (VGA + BIOS area, 384 KiB).
    entries.push(E820Entry {
        addr: 0xA_0000,
        size: 0x10_0000 - 0xA_0000,
        mem_type: E820_RESERVED,
    });

    // 3. High memory from HIMEM_START up to the MMIO gap (if RAM reaches it).
    if mem_size_bytes > HIMEM_START {
        let high_before_gap_end = mem_size_bytes.min(MMIO_GAP_START);
        entries.push(E820Entry {
            addr: HIMEM_START,
            size: high_before_gap_end - HIMEM_START,
            mem_type: E820_RAM,
        });
    }

    // 4. The complete MMIO gap. RAM that does not fit below it is relocated
    // above 4 GiB rather than being silently consumed by the aperture.
    if mem_size_bytes > MMIO_GAP_START {
        entries.push(E820Entry {
            addr: MMIO_GAP_START,
            size: MMIO_GAP_END - MMIO_GAP_START,
            mem_type: E820_RESERVED,
        });
        // 5. The packed bytes after the low slot are exposed above 4 GiB.
        entries.push(E820Entry {
            addr: MMIO_GAP_END,
            size: mem_size_bytes - MMIO_GAP_START,
            mem_type: E820_RAM,
        });
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e820_map_layout_small_16mib() {
        // 16 MiB RAM: low, reserved, high-before-gap. The gap is past the end.
        let m = build_e820_map(16 * 1024 * 1024);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].addr, 0);
        assert_eq!(m[0].size, 0xA_0000);
        assert_eq!(m[0].mem_type, E820_RAM);
        assert_eq!(m[1].addr, 0xA_0000);
        assert_eq!(m[1].mem_type, E820_RESERVED);
        assert_eq!(m[2].addr, HIMEM_START);
        assert_eq!(m[2].mem_type, E820_RAM);
        assert_eq!(m[2].size, 16 * 1024 * 1024 - HIMEM_START);
    }

    #[test]
    fn e820_map_layout_exactly_at_himem_start() {
        // RAM = HIMEM_START exactly: only low + reserved (no high RAM).
        let m = build_e820_map(HIMEM_START);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn e820_map_layout_large_4gib() {
        // A configured 4 GiB of packed RAM has 768 MiB relocated above 4 GiB.
        let m = build_e820_map(4 * 1024 * 1024 * 1024);
        assert_eq!(m.len(), 5);
        assert_eq!(m[3].addr, MMIO_GAP_START);
        assert_eq!(m[3].size, MMIO_GAP_END - MMIO_GAP_START);
        assert_eq!(m[3].mem_type, E820_RESERVED);
        assert_eq!(m[4].addr, MMIO_GAP_END);
        assert_eq!(m[4].size, 768 * 1024 * 1024);
        assert_eq!(m[4].mem_type, E820_RAM);
    }

    #[test]
    fn e820_map_layout_1gib_is_all_usable_outside_legacy_hole() {
        // Virtio MMIO starts at 3.25 GiB, so a 1 GiB guest must not silently
        // lose three quarters of its configured RAM to a fictitious hole.
        let m = build_e820_map(1024 * 1024 * 1024);
        assert_eq!(m.len(), 3);
        assert_eq!(m[2].addr, HIMEM_START);
        assert_eq!(m[2].size, 1024 * 1024 * 1024 - HIMEM_START);
        assert_eq!(m[2].mem_type, E820_RAM);
    }

    #[test]
    fn e820_ram_and_reserved_cover_the_physical_layout() {
        // Above the aperture, the physical span includes the inserted MMIO
        // gap while configured RAM remains packed.
        // We require mem_size >= HIMEM_START (1 MiB) — a real VMM never boots
        // with sub-megabyte RAM. Sizes at and above the gap are the
        // interesting boundary cases.
        for &sz in &[
            HIMEM_START,             // exactly 1 MiB (low + reserved, no high)
            16 * 1024 * 1024,        // before gap
            MMIO_GAP_START,          // exactly at gap start
            MMIO_GAP_START + 0x1000, // first high-memory page
            MMIO_GAP_END,
            256 * 1024 * 1024,      // ordinary small guest
            1024 * 1024 * 1024,     // 1 GiB, below the gap
            4 * 1024 * 1024 * 1024, // 4 GiB, exactly through the gap
        ] {
            let m = build_e820_map(sz);
            let total: u64 = m.iter().map(|e| e.size).sum();
            let expected = if sz > MMIO_GAP_START {
                sz + (MMIO_GAP_END - MMIO_GAP_START)
            } else {
                sz
            };
            assert_eq!(total, expected, "size 0x{sz:x}");
        }
    }

    #[test]
    fn e820_entries_are_contiguous_and_non_overlapping() {
        // Walk the entries; each must start where the previous ended.
        for &sz in &[16 * 1024 * 1024, 256 * 1024 * 1024, 1024 * 1024 * 1024] {
            let m = build_e820_map(sz);
            for w in m.windows(2) {
                assert_eq!(w[0].addr + w[0].size, w[1].addr, "size 0x{sz:x}");
            }
            assert_eq!(m[0].addr, 0);
            assert_eq!(m.last().unwrap().addr + m.last().unwrap().size, sz);
        }
    }
}
