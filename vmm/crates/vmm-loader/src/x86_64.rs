//! x86_64 boot protocol — zero-page (boot_params) construction + kernel load.
//!
//! This is the re-implementation of vmm-reference's `boot.rs` against modern
//! `linux-loader 0.14`. The kernel's x86 boot protocol is documented at
//! <https://www.kernel.org/doc/Documentation/x86/boot.txt>.
//!
//! The pure-Rust E820 map + constants live in [`crate::memmap`] (host-agnostic);
//! this module is the `boot_params`-shaped layer that's only meaningful on x86_64.

#![cfg(target_arch = "x86_64")]

use crate::error::{LoaderError, Result};
use crate::memmap::{
    build_e820_map, E820Entry, E820_RAM, HIMEM_START, KERNEL_BOOT_FLAG_MAGIC, KERNEL_HDR_MAGIC,
    KERNEL_LOADER_OTHER, KERNEL_MIN_ALIGNMENT_BYTES, MMIO_GAP_END, MMIO_GAP_START, ZERO_PAGE_ADDR,
};
use linux_loader::bootparam::boot_params;
use linux_loader::configurator::{BootConfigurator, BootParams};
use linux_loader::loader::KernelLoader as _;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

// Re-export the constants/tests consumers expect from this module.
pub use crate::memmap::{
    build_e820_map as _re_exported_build_e820_map, E820_RAM as _E820_RAM, E820_RESERVED,
    EBDA_START, HIMEM_START as _HIMEM_START, MMIO_GAP_END as _MMIO_GAP_END,
    MMIO_GAP_START as _MMIO_GAP_START, ZERO_PAGE_ADDR as _ZERO_PAGE_ADDR,
};

const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const BZIMAGE_MIN_HEADER_LEN: usize = 0x218;
const BZIMAGE_SETUP_SECTS_OFFSET: usize = 0x1f1;
const BZIMAGE_BOOT_FLAG_OFFSET: usize = 0x1fe;
const BZIMAGE_HEADER_OFFSET: usize = 0x202;
const BZIMAGE_VERSION_OFFSET: usize = 0x206;
const BZIMAGE_CODE32_START_OFFSET: usize = 0x214;
const BZIMAGE_BOOT_FLAG: u16 = 0xaa55;
const BZIMAGE_HEADER_MAGIC: &[u8; 4] = b"HdrS";
const MIN_BOOT_PROTOCOL_VERSION: u16 = 0x0200;
const MAX_KERNEL_IMAGE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_INITRAMFS_BYTES: u64 = 1024 * 1024 * 1024;
const DEFAULT_BZIMAGE_CODE32_START: u64 = 0x100000;
const DEFAULT_BZIMAGE_SETUP_SECTS: usize = 4;
const SECTOR_SIZE: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KernelImageType {
    Elf,
    BzImage(BzImageHeader),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BzImageHeader {
    setup_sects: usize,
    setup_size: usize,
    code32_start: u32,
}

/// Write the E820 map into the zero page's `e820_table` field (a fixed-size
/// array of `boot_e820_entry`). Returns the count of entries written.
///
/// The kernel's `boot_e820_entry` uses the field name `type` (a Rust
/// keyword), accessed here as `r#type`.
pub fn write_e820_into_zero_page(zero_page: &mut boot_params, entries: &[E820Entry]) -> usize {
    let max = zero_page.e820_table.len();
    let n = entries.len().min(max);
    for (i, e) in entries.iter().take(n).enumerate() {
        zero_page.e820_table[i] = linux_loader::bootparam::boot_e820_entry {
            addr: e.addr,
            size: e.size,
            r#type: e.mem_type,
        };
    }
    zero_page.e820_entries = n as u8;
    n
}

/// Build a populated `boot_params` (the zero page) ready for
/// `LinuxBootConfigurator::write_bootparams`.
///
/// `setup_header` comes from the loaded kernel image (bzImage path). For an
/// ELF `vmlinux` there's no setup_header; we synthesize a minimal one with
/// just the magic + loader fields.
pub fn build_zero_page(
    setup_header: Option<&linux_loader::bootparam::setup_header>,
    e820_entries: &[E820Entry],
) -> boot_params {
    let mut bp = boot_params::default();

    // Magic + header + our own loader stamp.
    bp.hdr.boot_flag = KERNEL_BOOT_FLAG_MAGIC;
    bp.hdr.header = KERNEL_HDR_MAGIC;
    bp.hdr.type_of_loader = KERNEL_LOADER_OTHER;
    bp.hdr.kernel_alignment = KERNEL_MIN_ALIGNMENT_BYTES;

    // Copy fields the kernel image provided (bzImage path), then re-stamp
    // the fields we own.
    if let Some(h) = setup_header {
        bp.hdr = *h;
        bp.hdr.boot_flag = KERNEL_BOOT_FLAG_MAGIC;
        bp.hdr.type_of_loader = KERNEL_LOADER_OTHER;
    }

    write_e820_into_zero_page(&mut bp, e820_entries);
    bp
}

/// Everything the VMM needs to boot: where the kernel landed, where the
/// zero page lives, where the cmdline + initramfs are.
#[derive(Debug, Clone)]
pub struct BootSetup {
    pub kernel_load: GuestAddress,
    pub kernel_end: GuestAddress,
    pub zero_page_addr: GuestAddress,
    pub cmdline_addr: GuestAddress,
    pub initramfs_addr: Option<GuestAddress>,
    pub initramfs_size: u64,
}

fn detect_kernel_image_type(file: &mut File) -> Result<KernelImageType> {
    file.seek(SeekFrom::Start(0))
        .map_err(|e| LoaderError::InvalidKernel(format!("seek kernel header: {e}")))?;
    let mut header = [0u8; BZIMAGE_MIN_HEADER_LEN];
    let bytes_read = read_prefix(file, &mut header)
        .map_err(|e| LoaderError::InvalidKernel(format!("read kernel header: {e}")))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| LoaderError::InvalidKernel(format!("rewind kernel: {e}")))?;

    if bytes_read >= ELF_MAGIC.len() && &header[..ELF_MAGIC.len()] == ELF_MAGIC {
        return Ok(KernelImageType::Elf);
    }
    if bytes_read >= BZIMAGE_MIN_HEADER_LEN {
        let bz_header = parse_bzimage_header(&header)?;
        return Ok(KernelImageType::BzImage(bz_header));
    }

    Err(LoaderError::InvalidKernel(
        "unrecognized kernel image: missing ELF magic and bzImage setup header".into(),
    ))
}

fn read_prefix(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut total = 0usize;
    while total < buf.len() {
        let n = file.read(&mut buf[total..])?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

fn parse_bzimage_header(header: &[u8; BZIMAGE_MIN_HEADER_LEN]) -> Result<BzImageHeader> {
    let boot_flag = u16::from_le_bytes([
        header[BZIMAGE_BOOT_FLAG_OFFSET],
        header[BZIMAGE_BOOT_FLAG_OFFSET + 1],
    ]);
    if boot_flag != BZIMAGE_BOOT_FLAG {
        return Err(LoaderError::InvalidKernel(format!(
            "bzImage boot flag mismatch: 0x{boot_flag:04x}"
        )));
    }
    if &header[BZIMAGE_HEADER_OFFSET..BZIMAGE_HEADER_OFFSET + BZIMAGE_HEADER_MAGIC.len()]
        != BZIMAGE_HEADER_MAGIC
    {
        return Err(LoaderError::InvalidKernel(
            "bzImage setup header magic missing".into(),
        ));
    }
    let protocol_version = u16::from_le_bytes([
        header[BZIMAGE_VERSION_OFFSET],
        header[BZIMAGE_VERSION_OFFSET + 1],
    ]);
    if protocol_version < MIN_BOOT_PROTOCOL_VERSION {
        return Err(LoaderError::InvalidKernel(format!(
            "bzImage boot protocol too old: 0x{protocol_version:04x}"
        )));
    }
    let setup_sects = if header[BZIMAGE_SETUP_SECTS_OFFSET] == 0 {
        DEFAULT_BZIMAGE_SETUP_SECTS
    } else {
        usize::from(header[BZIMAGE_SETUP_SECTS_OFFSET])
    };
    let setup_size = setup_sects
        .checked_add(1)
        .and_then(|sects| sects.checked_mul(SECTOR_SIZE))
        .ok_or_else(|| LoaderError::InvalidKernel("bzImage setup size overflows".into()))?;
    let code32_start = u32::from_le_bytes([
        header[BZIMAGE_CODE32_START_OFFSET],
        header[BZIMAGE_CODE32_START_OFFSET + 1],
        header[BZIMAGE_CODE32_START_OFFSET + 2],
        header[BZIMAGE_CODE32_START_OFFSET + 3],
    ]);

    Ok(BzImageHeader {
        setup_sects,
        setup_size,
        code32_start,
    })
}

fn validate_bzimage_payload(path: &Path, setup_size: usize, file_len: u64) -> Result<u64> {
    let setup_size_u64 = u64::try_from(setup_size)
        .map_err(|_| LoaderError::InvalidKernel("bzImage setup size overflows u64".into()))?;
    if file_len <= setup_size_u64 {
        return Err(LoaderError::InvalidKernel(format!(
            "bzImage {} has no kernel payload after {setup_size} setup bytes",
            path.display()
        )));
    }
    let kernel_payload_len = file_len - setup_size_u64;
    if kernel_payload_len > MAX_KERNEL_IMAGE_BYTES {
        return Err(LoaderError::InvalidKernel(format!(
            "kernel payload too large: {kernel_payload_len} > {MAX_KERNEL_IMAGE_BYTES}"
        )));
    }
    Ok(kernel_payload_len)
}

fn checked_usize_len(len: u64, what: &str) -> Result<usize> {
    usize::try_from(len).map_err(|_| LoaderError::Load(format!("{what} length overflows usize")))
}

fn checked_u32(value: u64, what: &str) -> Result<u32> {
    u32::try_from(value)
        .map_err(|_| LoaderError::BootConfig(format!("{what} exceeds 32-bit boot field: {value}")))
}

/// Load a kernel image and write the zero page + cmdline + initramfs into
/// guest memory.
///
/// Detects `vmlinux` (ELF) vs `bzImage` by image magic/setup header and
/// dispatches to the right loader.
pub fn load_and_setup_boot<P: AsRef<Path>>(
    guest_memory: &GuestMemoryMmap,
    kernel_path: P,
    cmdline_str: &str,
    initramfs_path: Option<P>,
    mem_size_bytes: u64,
) -> Result<BootSetup> {
    let path = kernel_path.as_ref();
    let mut kernel_file = std::fs::File::open(path)
        .map_err(|e| LoaderError::InvalidKernel(format!("open {}: {e}", path.display())))?;

    // Detect ELF vmlinux vs bzImage from magic/header bytes, not filenames.
    let image_type = detect_kernel_image_type(&mut kernel_file)?;
    let is_elf = matches!(image_type, KernelImageType::Elf);

    let loader_result = if matches!(image_type, KernelImageType::Elf) {
        linux_loader::loader::elf::Elf::load(guest_memory, None, &mut kernel_file, None)
            .map_err(|e| LoaderError::Load(format!("elf: {e:?}")))?
    } else {
        // bzImage: load manually (same approach as the standalone test that
        // works on c8i nested virt). linux-loader's BzImage::load does
        // extra processing that can cause KVM InternalError on nested virt.
        let KernelImageType::BzImage(bz_header) = image_type else {
            unreachable!("non-ELF image type must be bzImage");
        };
        if bz_header.code32_start != DEFAULT_BZIMAGE_CODE32_START as u32 {
            return Err(LoaderError::InvalidKernel(format!(
                "unsupported bzImage code32_start: 0x{:x}",
                bz_header.code32_start
            )));
        }
        let mut f = std::fs::File::open(path)
            .map_err(|e| LoaderError::InvalidKernel(format!("reopen {}: {e}", path.display())))?;
        let metadata = f
            .metadata()
            .map_err(|e| LoaderError::InvalidKernel(format!("stat {}: {e}", path.display())))?;
        let kernel_payload_len =
            validate_bzimage_payload(path, bz_header.setup_size, metadata.len())?;

        // Load the setup code at 0x10000.
        f.seek(SeekFrom::Start(0))
            .map_err(|e| LoaderError::Load(format!("seek setup: {e}")))?;
        let mut setup_buf = vec![0u8; bz_header.setup_size];
        f.read_exact(&mut setup_buf)
            .map_err(|e| LoaderError::Load(format!("read setup: {e}")))?;
        guest_memory
            .write_slice(&setup_buf, GuestAddress(0x10000))
            .map_err(|e| LoaderError::Load(format!("write setup: {e:?}")))?;

        // Load the compressed kernel at code32_start (0x100000 = 1 MiB).
        // The setup header's code32_start field (offset 0x214) points here —
        // it's where the setup code jumps after switching to 32-bit protected
        // mode. Loading at a different address (e.g. 0x200000) means the
        // setup code jumps to empty memory and hangs.
        let kernel_load_addr = DEFAULT_BZIMAGE_CODE32_START;
        f.seek(SeekFrom::Start(bz_header.setup_size as u64))
            .map_err(|e| LoaderError::Load(format!("seek kernel: {e}")))?;
        let kernel_payload_len_usize = checked_usize_len(kernel_payload_len, "kernel payload")?;
        let mut kernel_data = vec![0u8; kernel_payload_len_usize];
        f.read_exact(&mut kernel_data)
            .map_err(|e| LoaderError::Load(format!("read kernel: {e}")))?;
        guest_memory
            .write_slice(&kernel_data, GuestAddress(kernel_load_addr))
            .map_err(|e| LoaderError::Load(format!("write kernel: {e:?}")))?;

        // Return a minimal KernelLoaderResult (no setup_header — we patched
        // the setup code directly).
        linux_loader::loader::KernelLoaderResult {
            kernel_load: GuestAddress(kernel_load_addr),
            kernel_end: kernel_data.len() as u64,
            setup_header: None,
            pvh_boot_cap: linux_loader::loader::elf::PvhBootCapability::PvhEntryNotPresent,
        }
    };

    // For bzImage, the entry point is the 32-bit protected-mode `startup_32`
    // at code32_start (where we loaded the compressed kernel, 0x100000).
    // For ELF vmlinux, always use the ELF entry point (startup_64) with
    // the LinuxBoot protocol. Firecracker does the same — it doesn't use
    // the PVH entry point even when the PVH ELF note is present.
    let kernel_load = loader_result.kernel_load;
    let kernel_end = GuestAddress(loader_result.kernel_load.raw_value() + loader_result.kernel_end);

    // Validate + assemble the cmdline via linux-loader's `Cmdline` (enforces
    // the kernel's 4096-byte zero-page limit), then convert to bytes.
    let mut cmdline = linux_loader::cmdline::Cmdline::new(crate::cmdline::CMDLINE_MAX)
        .map_err(|e| LoaderError::BootConfig(format!("cmdline: {e:?}")))?;
    cmdline
        .insert_str(cmdline_str)
        .map_err(|e| LoaderError::BootConfig(format!("cmdline insert: {e:?}")))?;

    // Place the cmdline in low RAM, 128 KiB above HIMEM_START (a known-free
    // region). The zero page's `hdr.cmd_line_ptr` will point here. The zero
    // page itself goes to ZERO_PAGE_ADDR so the two don't collide.
    let cmdline_load_addr = GuestAddress(HIMEM_START + 0x_0002_0000);
    let cmdline_bytes: Vec<u8> = std::convert::TryFrom::try_from(cmdline)
        .map_err(|e| LoaderError::BootConfig(format!("cmdline to bytes: {e:?}")))?;
    guest_memory
        .write_slice(&cmdline_bytes, cmdline_load_addr)
        .map_err(|e| LoaderError::BootConfig(format!("cmdline write: {e:?}")))?;

    // Place the initramfs, if any, at a high address that's definitely
    // above all kernel segments. The ELF vmlinux can have segments spread
    // across a large range — using `kernel_end + page_align` is not safe
    // because `kernel_end` may not account for all ELF segments. Use a
    // fixed address at 128 MiB (well above the ~42 MiB kernel image).
    let mut initramfs_addr = None;
    let mut initramfs_size = 0u64;
    if let Some(ir_path) = initramfs_path {
        let ir = ir_path.as_ref();
        let metadata = std::fs::metadata(ir)
            .map_err(|e| LoaderError::Initramfs(format!("stat {}: {e}", ir.display())))?;
        initramfs_size = metadata.len();
        if initramfs_size > MAX_INITRAMFS_BYTES {
            return Err(LoaderError::Initramfs(format!(
                "initramfs too large: {initramfs_size} > {MAX_INITRAMFS_BYTES}"
            )));
        }
        let ir_addr = GuestAddress(0x800_0000); // 128 MiB — above all kernel segments
        let mut ir_file = std::fs::File::open(ir)
            .map_err(|e| LoaderError::Initramfs(format!("open {}: {e}", ir.display())))?;
        let initramfs_len = usize::try_from(initramfs_size).map_err(|_| {
            LoaderError::Initramfs(format!(
                "initramfs length overflows usize: {initramfs_size}"
            ))
        })?;
        let mut buf = vec![0u8; initramfs_len];
        ir_file
            .read_exact(&mut buf)
            .map_err(|e| LoaderError::Initramfs(format!("read {}: {e}", ir.display())))?;
        guest_memory
            .write_slice(&buf, ir_addr)
            .map_err(|e| LoaderError::Initramfs(format!("write: {e:?}")))?;
        initramfs_addr = Some(ir_addr);
    }

    // Build the E820 map and the zero page.
    let e820 = build_e820_map(mem_size_bytes);

    // Check if the ELF kernel supports PVH boot. Firecracker always uses
    // the LinuxBoot protocol on x86_64 (even when the PVH ELF note is
    // present), so we do the same.
    let use_pvh = false;

    // Boot params address: 0x7000 for ELF vmlinux (matching Firecracker's
    // ZERO_PAGE_START), 0x10000 for bzImage (setup header location).
    let zp_addr: u64 = if is_elf { 0x7000 } else { ZERO_PAGE_ADDR };

    if is_elf {
        // ELF vmlinux: use PVH if the kernel supports it (CONFIG_PVH=y),
        // otherwise use the Linux boot protocol (boot_params).
        if use_pvh {
            // PVH boot: write hvm_start_info at 0x6000.
            use linux_loader::configurator::pvh::PvhBootConfigurator;
            use linux_loader::loader::elf::start_info::{
                hvm_memmap_table_entry, hvm_modlist_entry, hvm_start_info,
            };

            const PVH_INFO_START: u64 = 0x6000;
            let memmap_addr = PVH_INFO_START + std::mem::size_of::<hvm_start_info>() as u64;
            let memmap_bytes = e820.len() * std::mem::size_of::<hvm_memmap_table_entry>();
            let modlist_addr = memmap_addr + memmap_bytes as u64;

            let (nr_modules, modlist_paddr) = if let Some(ir_addr) = initramfs_addr {
                let entry = hvm_modlist_entry {
                    paddr: ir_addr.0,
                    size: initramfs_size,
                    cmdline_paddr: 0,
                    reserved: 0,
                };
                let entry_bytes = unsafe {
                    std::slice::from_raw_parts(
                        &entry as *const hvm_modlist_entry as *const u8,
                        std::mem::size_of::<hvm_modlist_entry>(),
                    )
                };
                guest_memory
                    .write_slice(entry_bytes, GuestAddress(modlist_addr))
                    .map_err(|e| LoaderError::BootConfig(format!("write modlist: {e:?}")))?;
                (1u32, modlist_addr)
            } else {
                (0u32, 0u64)
            };

            let start_info = hvm_start_info {
                magic: 0x336ec578,
                version: 1,
                flags: 0,
                nr_modules,
                modlist_paddr,
                cmdline_paddr: cmdline_load_addr.0,
                rsdp_paddr: 0,
                memmap_paddr: memmap_addr,
                memmap_entries: e820.len() as u32,
                reserved: 0,
            };

            let memmap: Vec<hvm_memmap_table_entry> = e820
                .iter()
                .map(|e| hvm_memmap_table_entry {
                    addr: e.addr,
                    size: e.size,
                    type_: e.mem_type,
                    reserved: 0,
                })
                .collect();

            let sections_bytes = unsafe {
                std::slice::from_raw_parts(
                    memmap.as_ptr() as *const u8,
                    memmap.len() * std::mem::size_of::<hvm_memmap_table_entry>(),
                )
            };

            let boot_params = BootParams::new(&start_info, GuestAddress(PVH_INFO_START));
            let mut bp = boot_params;
            bp.sections = Some(sections_bytes.to_vec());
            bp.sections_start = Some(GuestAddress(start_info.memmap_paddr));
            PvhBootConfigurator::write_bootparams(&bp, guest_memory)
                .map_err(|e| LoaderError::BootConfig(format!("pvh write_bootparams: {e:?}")))?;
        } else {
            // LinuxBoot: write boot_params at ZERO_PAGE_ADDR (0x10000).
            // The kernel's startup_64 reads RSI as a pointer to boot_params.
            let bp = build_zero_page(None, &e820);
            use linux_loader::configurator::linux::LinuxBootConfigurator;
            let boot_params = BootParams::new(&bp, GuestAddress(zp_addr));
            LinuxBootConfigurator::write_bootparams(&boot_params, guest_memory)
                .map_err(|e| LoaderError::BootConfig(format!("linux write_bootparams: {e:?}")))?;

            // Patch cmd_line_ptr, initramfs, and E820 (the configurator
            // writes the full boot_params which may overwrite these).
            guest_memory
                .write_obj(cmdline_load_addr.0 as u32, GuestAddress(zp_addr + 0x228))
                .map_err(|e| LoaderError::BootConfig(format!("write cmd_line_ptr: {e:?}")))?;

            if let Some(ir_addr) = initramfs_addr {
                let ramdisk_size = checked_u32(initramfs_size, "initramfs_size")?;
                guest_memory
                    .write_obj(ir_addr.0 as u32, GuestAddress(zp_addr + 0x218))
                    .map_err(|e| LoaderError::BootConfig(format!("write ramdisk_image: {e:?}")))?;
                guest_memory
                    .write_obj(ramdisk_size, GuestAddress(zp_addr + 0x21c))
                    .map_err(|e| LoaderError::BootConfig(format!("write ramdisk_size: {e:?}")))?;
            }

            let e820_count = e820.len().min(128) as u8;
            guest_memory
                .write_obj(e820_count, GuestAddress(zp_addr + 0x1e8))
                .map_err(|e| LoaderError::BootConfig(format!("write e820_entries: {e:?}")))?;
            for (i, entry) in e820.iter().take(128).enumerate() {
                let entry_offset = zp_addr + 0x2d0 + (i as u64) * 20;
                let mut buf = [0u8; 20];
                buf[0..8].copy_from_slice(&entry.addr.to_le_bytes());
                buf[8..16].copy_from_slice(&entry.size.to_le_bytes());
                buf[16..20].copy_from_slice(&entry.mem_type.to_le_bytes());
                guest_memory
                    .write_slice(&buf, GuestAddress(entry_offset))
                    .map_err(|e| LoaderError::BootConfig(format!("write e820[{i}]: {e:?}")))?;
            }
        }
    } else {
        // bzImage: the zero page IS the setup code's header at 0x10000.
        // The setup code's header occupies the first ~2 KiB. The E820 table
        // lives at offset 0x2d0 in boot_params, and the E820 entry count at
        // offset 0x1e8. These offsets are within the setup code's header
        // area (the first 0x290 bytes of the header are defined; the E820
        // table at 0x2d0 is after the setup_header but still within the
        // boot_params struct that the setup code preserves).
        //
        // The kernel's setup code copies boot_params to a safe location before
        // decompressing, so writing E820 here is safe — the setup code passes
        // the full boot_params through to the decompressed kernel.
        guest_memory
            .write_obj(cmdline_load_addr.0 as u32, GuestAddress(zp_addr + 0x228))
            .map_err(|e| LoaderError::BootConfig(format!("write cmd_line_ptr: {e:?}")))?;
        if let Some(ir_addr) = initramfs_addr {
            let ramdisk_size = checked_u32(initramfs_size, "initramfs_size")?;
            guest_memory
                .write_obj(ir_addr.0 as u32, GuestAddress(zp_addr + 0x218))
                .map_err(|e| LoaderError::BootConfig(format!("write ramdisk_image: {e:?}")))?;
            guest_memory
                .write_obj(ramdisk_size, GuestAddress(zp_addr + 0x21c))
                .map_err(|e| LoaderError::BootConfig(format!("write ramdisk_size: {e:?}")))?;
        }

        // Write the E820 map into the zero page's e820_table (offset 0x2d0)
        // and e820_entries count (offset 0x1e8). The kernel needs this to know
        // the memory layout.
        let e820_count = e820.len().min(128) as u8; // e820_table has 128 entries
        guest_memory
            .write_obj(e820_count, GuestAddress(zp_addr + 0x1e8))
            .map_err(|e| LoaderError::BootConfig(format!("write e820_entries: {e:?}")))?;

        for (i, entry) in e820.iter().take(128).enumerate() {
            let entry_offset = zp_addr + 0x2d0 + (i as u64) * 20;
            // boot_e820_entry: u64 addr, u64 size, u32 type (20 bytes total)
            let mut buf = [0u8; 20];
            buf[0..8].copy_from_slice(&entry.addr.to_le_bytes());
            buf[8..16].copy_from_slice(&entry.size.to_le_bytes());
            buf[16..20].copy_from_slice(&entry.mem_type.to_le_bytes());
            guest_memory
                .write_slice(&buf, GuestAddress(entry_offset))
                .map_err(|e| LoaderError::BootConfig(format!("write e820[{i}]: {e:?}")))?;
        }

        // Set pref_address (offset 0x230) — the preferred load address for
        // the real-mode code. Standard value is 0x100000 (1 MiB).
        guest_memory
            .write_obj(0x100000u64, GuestAddress(zp_addr + 0x230))
            .map_err(|e| LoaderError::BootConfig(format!("write pref_address: {e:?}")))?;

        // Set init_size (offset 0x238) — the kernel initialization size.
        // The decompressor uses this to calculate memory allocation.
        // Without it, the decompressor runs out of memory.
        // Use a generous default (64 MiB) that covers the decompressed kernel
        // plus heap overhead.
        guest_memory
            .write_obj(0x400_0000u32, GuestAddress(zp_addr + 0x238))
            .map_err(|e| LoaderError::BootConfig(format!("write init_size: {e:?}")))?;
    }

    // Silence the unused-import warnings for re-exported constants above;
    // they're here so consumers can `use crate::x86_64::*` like vmm-reference.
    let _ = (MMIO_GAP_START, MMIO_GAP_END, E820_RAM);

    // Always use the ELF entry point (startup_64) for LinuxBoot.
    let entry = kernel_load;

    Ok(BootSetup {
        kernel_load: entry,
        kernel_end,
        zero_page_addr: GuestAddress(zp_addr),
        cmdline_addr: cmdline_load_addr,
        initramfs_addr,
        initramfs_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a field from a packed struct safely (avoiding unaligned refs).
    /// `boot_params` and `boot_e820_entry` are `#[repr(C, packed)]`, so
    /// direct field access via `&zp.hdr.x` is UB. We copy each field to a
    /// local before comparing.
    macro_rules! packed_read {
        ($obj:expr, $field:ident) => {{
            // SAFETY: packed-struct field read via pointer is the documented
            // way to avoid creating an unaligned reference.
            let p: *const _ = &$obj;
            unsafe { std::ptr::addr_of!((*p).$field).read() }
        }};
    }

    #[test]
    fn zero_page_has_magic_and_loader() {
        let e820 = build_e820_map(256 * 1024 * 1024);
        let zp = build_zero_page(None, &e820);
        assert_eq!(packed_read!(zp.hdr, boot_flag), KERNEL_BOOT_FLAG_MAGIC);
        assert_eq!(packed_read!(zp.hdr, header), KERNEL_HDR_MAGIC);
        assert_eq!(packed_read!(zp.hdr, type_of_loader), KERNEL_LOADER_OTHER);
        assert_eq!(
            packed_read!(zp.hdr, kernel_alignment),
            KERNEL_MIN_ALIGNMENT_BYTES
        );
    }

    #[test]
    fn zero_page_e820_count_matches_entries() {
        let e820 = build_e820_map(1024 * 1024 * 1024);
        let zp = build_zero_page(None, &e820);
        assert_eq!(packed_read!(zp, e820_entries) as usize, e820.len());
        let first = &zp.e820_table[0];
        assert_eq!(packed_read!(first, addr), 0);
        assert_eq!(packed_read!(first, r#type), E820_RAM);
    }

    #[test]
    fn write_e820_truncates_to_array_capacity() {
        let mut zp = boot_params::default();
        let cap = zp.e820_table.len();
        let big: Vec<E820Entry> = (0..(cap + 5))
            .map(|i| E820Entry {
                addr: i as u64,
                size: 0x1000,
                mem_type: E820_RAM,
            })
            .collect();
        let n = write_e820_into_zero_page(&mut zp, &big);
        assert_eq!(n, cap);
        assert_eq!(packed_read!(zp, e820_entries) as usize, cap);
    }

    #[test]
    fn parse_bzimage_header_validates_magic() {
        let mut header = [0u8; BZIMAGE_MIN_HEADER_LEN];
        header[BZIMAGE_BOOT_FLAG_OFFSET..BZIMAGE_BOOT_FLAG_OFFSET + 2]
            .copy_from_slice(&BZIMAGE_BOOT_FLAG.to_le_bytes());
        header[BZIMAGE_HEADER_OFFSET..BZIMAGE_HEADER_OFFSET + 4]
            .copy_from_slice(BZIMAGE_HEADER_MAGIC);
        header[BZIMAGE_VERSION_OFFSET..BZIMAGE_VERSION_OFFSET + 2]
            .copy_from_slice(&MIN_BOOT_PROTOCOL_VERSION.to_le_bytes());
        header[BZIMAGE_CODE32_START_OFFSET..BZIMAGE_CODE32_START_OFFSET + 4]
            .copy_from_slice(&(DEFAULT_BZIMAGE_CODE32_START as u32).to_le_bytes());

        let parsed = parse_bzimage_header(&header).unwrap();
        assert_eq!(parsed.setup_sects, DEFAULT_BZIMAGE_SETUP_SECTS);
        assert_eq!(
            parsed.setup_size,
            (DEFAULT_BZIMAGE_SETUP_SECTS + 1) * SECTOR_SIZE
        );

        header[BZIMAGE_HEADER_OFFSET] = b'X';
        assert!(parse_bzimage_header(&header).is_err());
    }

    #[test]
    fn validate_bzimage_payload_rejects_empty_payload() {
        let setup_size = (DEFAULT_BZIMAGE_SETUP_SECTS + 1) * SECTOR_SIZE;
        assert!(
            validate_bzimage_payload(Path::new("kernel"), setup_size, setup_size as u64).is_err()
        );
    }
}
