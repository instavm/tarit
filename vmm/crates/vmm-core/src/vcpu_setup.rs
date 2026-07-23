// Provenance and modification notice:
//
// Parts of this file (the x86_64 GDT layout, the DSDT AML bytes, and the boot
// MSR sequence) are adapted to match Firecracker's boot protocol and overlap
// with Firecracker's x86_64 arch code. Firecracker is Copyright Amazon.com,
// Inc. or its affiliates and is licensed under Apache-2.0; some of those
// snippets derive in turn from the Chromium OS project under a BSD-style
// license. These portions have been adapted and modified for Tarit. See the
// root NOTICE file for the full attribution. The Apache-2.0 and BSD notices
// are retained for the derived portions; the rest of this file is original
// Tarit code under the project license.

//! x86_64 vCPU register setup for direct kernel boot.
//!
//! Two boot modes:
//! - **Fast boot** (`full_boot=false`): 32-bit protected mode, no IRQCHIP.
//!   The kernel HLTs quickly. Used for benchmarks and unit tests.
//! - **Full boot** (`full_boot=true`): 64-bit long mode with IRQCHIP, PIT,
//!   ACPI tables, CPUID, LAPIC. The kernel reaches init and runs userspace.
//!   Follows the standard x86_64 direct kernel boot protocol.

#![cfg(all(feature = "kvm", target_arch = "x86_64", target_os = "linux"))]

use crate::cpu_template::CpuTemplate;
use crate::error::{Result, VmmError};
use kvm_bindings::kvm_segment;
use kvm_ioctls::VcpuFd;
use vm_memory::{Bytes, GuestAddress};
use vmm_loader::LoadedKernel;
use vmm_memory_backend::GuestMemory;

// ============================================================
// 32-bit fast boot constants
// ============================================================

const GDT_ENTRY_CODE32: u64 = 0x00cf_9a00_0000_ffff;
const GDT_ENTRY_DATA: u64 = 0x00cf_9200_0000_ffff;
const GDT32: [u64; 3] = [0, GDT_ENTRY_CODE32, GDT_ENTRY_DATA];
const GDT32_ADDR: u64 = 0x0050_0000;

// ============================================================
// 64-bit full boot constants
// ============================================================

const BOOT_GDT: [u64; 4] = [
    0,                     // NULL
    0x00af_9b00_0000_ffff, // CODE (gdt_entry(0xa09b, 0, 0xfffff))
    0x00cf_9300_0000_ffff, // DATA (gdt_entry(0xc093, 0, 0xfffff))
    0x008f_8b00_0000_ffff, // TSS (gdt_entry(0x808b, 0, 0xfffff))
];
const BOOT_GDT_ADDR: u64 = 0x500;
const BOOT_IDT_ADDR: u64 = 0x520;

/// Page table addresses.
const PML4_ADDR: u64 = 0x9000;
const PDPTE_ADDR: u64 = 0xa000;
const PDE_ADDR: u64 = 0xb000;

/// EFER / CR0 / CR4 bits.
const EFER_LME: u64 = 0x100;
const EFER_LMA: u64 = 0x400;
const CR0_PE: u64 = 1 << 0; // Protection Enable
const CR0_MP: u64 = 1 << 1; // Monitor Coprocessor
const CR0_ET: u64 = 1 << 4; // Extension Type (387 FPU present)
const CR0_NE: u64 = 1 << 5; // Numeric Error
const CR0_PG: u64 = 1 << 31; // Paging
const CR4_PAE: u64 = 1 << 5;

/// ACPI RSDP address.
const RSDP_ADDR: u64 = 0xe0000;

// ============================================================
// Cached CPUID
// ============================================================

static CACHED_CPUID: std::sync::Mutex<Option<kvm_bindings::CpuId>> = std::sync::Mutex::new(None);

pub fn setup_cpuid(vcpu: &VcpuFd) -> Result<()> {
    setup_cpuid_with_template(vcpu, &CpuTemplate::bare())
}

pub(crate) fn setup_cpuid_with_template(vcpu: &VcpuFd, template: &CpuTemplate) -> Result<()> {
    let cpuid = {
        let mut guard = CACHED_CPUID.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            let kvm = kvm_ioctls::Kvm::new()
                .map_err(|e| VmmError::Kvm(format!("Kvm::new for cpuid: {e}")))?;
            let c = kvm
                .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
                .map_err(|e| VmmError::Kvm(format!("KVM_GET_SUPPORTED_CPUID: {e}")))?;
            *guard = Some(c);
        }
        guard
            .as_ref()
            .expect("cached CPUID is initialized before use")
            .clone()
    };

    let mut cpuid = cpuid;
    normalize_boot_cpuid(&mut cpuid, 0);
    apply_cpuid_template(&mut cpuid, template);

    vcpu.set_cpuid2(&cpuid)
        .map_err(|e| VmmError::Kvm(format!("KVM_SET_CPUID2: {e}")))?;
    Ok(())
}

fn normalize_boot_cpuid(cpuid: &mut kvm_bindings::CpuId, apic_id: u8) {
    for entry in cpuid.as_mut_slice().iter_mut() {
        if entry.function == 1 && entry.index == 0 {
            entry.ebx = (entry.ebx & 0x00FF_FFFF) | ((apic_id as u32) << 24);
            entry.edx |= 1 << 9; // APIC
            entry.ecx |= 1 << 24; // TSC-Deadline
            entry.ecx &= !(1 << 21); // mask x2APIC (nested-virt safety)
            break;
        }
    }
}

fn apply_cpuid_template(cpuid: &mut kvm_bindings::CpuId, template: &CpuTemplate) {
    if template.cpuid.is_empty() {
        return;
    }

    for mask in &template.cpuid {
        for entry in cpuid.as_mut_slice().iter_mut() {
            if entry.function == mask.leaf && entry.index == mask.subleaf {
                entry.eax &= mask.eax;
                entry.ebx &= mask.ebx;
                entry.ecx &= mask.ecx;
                entry.edx &= mask.edx;
            }
        }
    }
}

fn check_msr_count(op: &str, got: usize, want: usize) -> Result<()> {
    if got != want {
        return Err(VmmError::Kvm(format!(
            "{op}: KVM processed {got}/{want} MSRs"
        )));
    }
    Ok(())
}

fn set_msrs_checked(vcpu: &VcpuFd, msrs: &kvm_bindings::Msrs, op: &str) -> Result<()> {
    let want = msrs.as_slice().len();
    let got = vcpu
        .set_msrs(msrs)
        .map_err(|e| VmmError::Kvm(format!("{op}: {e}")))?;
    check_msr_count(op, got, want)
}

fn get_msrs_checked(vcpu: &VcpuFd, msrs: &mut kvm_bindings::Msrs, op: &str) -> Result<()> {
    let want = msrs.as_slice().len();
    let got = vcpu
        .get_msrs(msrs)
        .map_err(|e| VmmError::Kvm(format!("{op}: {e}")))?;
    check_msr_count(op, got, want)
}

pub(crate) fn apply_msr_template(vcpu: &VcpuFd, template: &CpuTemplate) -> Result<()> {
    let mut clear_masks: Vec<(u32, u64)> = Vec::new();
    for &(index, mask) in &template.msr_clear {
        if mask == 0 {
            continue;
        }
        if let Some((_, existing)) = clear_masks
            .iter_mut()
            .find(|(existing_index, _)| *existing_index == index)
        {
            *existing |= mask;
        } else {
            clear_masks.push((index, mask));
        }
    }
    if clear_masks.is_empty() {
        return Ok(());
    }

    let entries: Vec<kvm_bindings::kvm_msr_entry> = clear_masks
        .iter()
        .map(|&(index, _)| kvm_bindings::kvm_msr_entry {
            index,
            ..Default::default()
        })
        .collect();
    let mut msrs = kvm_bindings::Msrs::from_entries(&entries)
        .map_err(|e| VmmError::Kvm(format!("Msrs::from_entries(cpu template): {e:?}")))?;
    get_msrs_checked(vcpu, &mut msrs, "KVM_GET_MSRS(cpu template)")?;

    for entry in msrs.as_mut_slice().iter_mut() {
        if let Some((_, clear_mask)) = clear_masks
            .iter()
            .find(|(existing_index, _)| *existing_index == entry.index)
        {
            entry.data &= !*clear_mask;
        }
    }
    set_msrs_checked(vcpu, &msrs, "KVM_SET_MSRS(cpu template)")
}

// ============================================================
// LAPIC LVT configuration
// ============================================================

pub fn set_lint(vcpu: &VcpuFd) -> Result<()> {
    let mut klapic = vcpu
        .get_lapic()
        .map_err(|e| VmmError::Kvm(format!("KVM_GET_LAPIC: {e}")))?;

    const APIC_LVT0: usize = 0x350;
    const APIC_LVT1: usize = 0x360;
    const APIC_MODE_EXTINT: u32 = 0x7;
    const APIC_MODE_NMI: u32 = 0x4;

    fn set_delivery_mode(regs: &mut [i8; 1024], offset: usize, mode: u32) {
        let val = u32::from_le_bytes([
            regs[offset] as u8,
            regs[offset + 1] as u8,
            regs[offset + 2] as u8,
            regs[offset + 3] as u8,
        ]);
        let val = (val & !0x700) | (mode << 8);
        let bytes = val.to_le_bytes();
        regs[offset] = bytes[0] as i8;
        regs[offset + 1] = bytes[1] as i8;
        regs[offset + 2] = bytes[2] as i8;
        regs[offset + 3] = bytes[3] as i8;
    }

    set_delivery_mode(&mut klapic.regs, APIC_LVT0, APIC_MODE_EXTINT);
    set_delivery_mode(&mut klapic.regs, APIC_LVT1, APIC_MODE_NMI);

    vcpu.set_lapic(&klapic)
        .map_err(|e| VmmError::Kvm(format!("KVM_SET_LAPIC: {e}")))?;
    Ok(())
}

// ============================================================
// ACPI tables
// ============================================================

pub fn write_acpi_tables(mem: &GuestMemory, nr_vcpus: u8) -> Result<()> {
    write_acpi_tables_with_devices(mem, nr_vcpus, &[])
}

/// Write ACPI tables including DSDT entries for virtio-mmio devices.
/// Each device entry has (mmio_addr, mmio_size, gsi_irq).
pub fn write_acpi_tables_with_devices(
    mem: &GuestMemory,
    nr_vcpus: u8,
    devices: &[(u64, u64, u32)],
) -> Result<()> {
    use acpi_tables::fadt::{
        FADT_F_HW_REDUCED_ACPI, FADT_F_PWR_BUTTON, FADT_F_SLP_BUTTON,
        IAPC_BOOT_ARG_FLAGS_VGA_NOT_PRESENT,
    };
    use acpi_tables::madt::{IoAPIC, LocalAPIC};
    use acpi_tables::{Dsdt, Fadt, Madt, Mcfg, Rsdp, Sdt, Xsdt};
    use zerocopy::IntoBytes;

    const OEM_ID: [u8; 6] = *b"INSTAV";
    const OEM_REVISION: u32 = 0;
    const IOAPIC_ADDR: u32 = 0xfec0_0000;
    const LAPIC_ADDR: u32 = 0xfee0_0000;
    const PCI_MMCONFIG_START: u64 = IOAPIC_ADDR as u64 - (256 << 20);

    fn map_acpi(e: acpi_tables::AcpiError) -> VmmError {
        VmmError::Memory(format!("ACPI table: {e}"))
    }

    // Fixed guest addresses for ACPI tables (below RSDP at 0xe0000).
    let madt_addr: u64 = 0xe1000;
    let xsdt_addr: u64 = 0xe2000;
    let dsdt_addr: u64 = 0xe3000;
    let fadt_addr: u64 = 0xe4000;
    let mcfg_addr: u64 = 0xe5000;

    // DSDT: virtio first, legacy COM1/PS2 last.
    let dsdt_body = build_dsdt(devices)?;
    let mut dsdt = Dsdt::new(OEM_ID, *b"IVMMDSDT", OEM_REVISION, dsdt_body);
    dsdt.write_to_guest(mem.inner.as_ref(), GuestAddress(dsdt_addr))
        .map_err(map_acpi)?;

    // FADT with HW_REDUCED_ACPI + VGA_NOT_PRESENT; DSDT pointer via X_DSDT only.
    let mut fadt = Fadt::new(OEM_ID, *b"IVMMFADT", OEM_REVISION);
    fadt.set_hypervisor_vendor_id(*b"VMMHV\0\0\0");
    fadt.set_x_dsdt(dsdt_addr);
    fadt.set_flags(
        (1 << FADT_F_HW_REDUCED_ACPI) | (1 << FADT_F_PWR_BUTTON) | (1 << FADT_F_SLP_BUTTON),
    );
    fadt.setup_iapc_flags(1 << IAPC_BOOT_ARG_FLAGS_VGA_NOT_PRESENT);
    fadt.write_to_guest(mem.inner.as_ref(), GuestAddress(fadt_addr))
        .map_err(map_acpi)?;

    // MADT: IOAPIC + local APIC per vCPU.
    let mut interrupt_controllers = Vec::new();
    interrupt_controllers.extend_from_slice(IoAPIC::new(0, IOAPIC_ADDR).as_bytes());
    for i in 0..nr_vcpus {
        interrupt_controllers.extend_from_slice(LocalAPIC::new(i).as_bytes());
    }
    let mut madt = Madt::new(
        OEM_ID,
        *b"IVMMMADT",
        OEM_REVISION,
        LAPIC_ADDR,
        interrupt_controllers,
    );
    madt.write_to_guest(mem.inner.as_ref(), GuestAddress(madt_addr))
        .map_err(map_acpi)?;

    // MCFG (always included in the XSDT).
    let mut mcfg = Mcfg::new(OEM_ID, *b"IVMMMCFG", OEM_REVISION, PCI_MMCONFIG_START);
    mcfg.write_to_guest(mem.inner.as_ref(), GuestAddress(mcfg_addr))
        .map_err(map_acpi)?;

    // XSDT: FADT + MADT + MCFG (DSDT is referenced from FADT only).
    let mut xsdt = Xsdt::new(
        OEM_ID,
        *b"IVMMXSDT",
        OEM_REVISION,
        vec![fadt_addr, madt_addr, mcfg_addr],
    );
    xsdt.write_to_guest(mem.inner.as_ref(), GuestAddress(xsdt_addr))
        .map_err(map_acpi)?;

    // RSDP → XSDT.
    let mut rsdp = Rsdp::new(OEM_ID, xsdt_addr);
    rsdp.write_to_guest(mem.inner.as_ref(), GuestAddress(RSDP_ADDR))
        .map_err(map_acpi)?;

    log::info!(
        "ACPI tables: RSDP@0x{RSDP_ADDR:x}, XSDT@0x{xsdt_addr:x}, FADT@0x{fadt_addr:x}, MADT@0x{madt_addr:x}, MCFG@0x{mcfg_addr:x}, DSDT@0x{dsdt_addr:x}"
    );
    Ok(())
}

/// Build a minimal DSDT AML body with Device() entries for virtio-mmio.
/// Uses the conventional microVM DSDT layout: virtio devices first, then
/// the legacy port-IO devices.
fn build_dsdt(devices: &[(u64, u64, u32)]) -> Result<Vec<u8>> {
    let mut aml = Vec::new();
    // Virtio first.
    for &(mmio_addr, mmio_size, gsi) in devices {
        append_virtio_aml(&mut aml, mmio_addr, mmio_size, gsi)?;
    }
    append_legacy_aml(&mut aml)?;
    Ok(aml)
}

/// First legacy GSIs start at 5 (IRQ0–4 are timer/kbd/cascade/COM1).
const GSI_LEGACY_START: u32 = 5;

/// COM1 + i8042 PS/2 AML in the standard legacy-device layout (adapted
/// code; see the provenance note at the top of this file).
fn append_legacy_aml(aml: &mut Vec<u8>) -> Result<()> {
    use acpi_tables::aml::{self, AmlError};
    use acpi_tables::Aml;

    fn map_aml(e: AmlError) -> VmmError {
        VmmError::Memory(format!("AML encode: {e}"))
    }

    aml::Device::new(
        "_SB_.COM1".try_into().map_err(map_aml)?,
        vec![
            &aml::Name::new(
                "_HID".try_into().map_err(map_aml)?,
                &aml::EisaName::new("PNP0501").map_err(map_aml)?,
            )
            .map_err(map_aml)?,
            &aml::Name::new("_UID".try_into().map_err(map_aml)?, &0u8).map_err(map_aml)?,
            &aml::Name::new("_DDN".try_into().map_err(map_aml)?, &"COM1").map_err(map_aml)?,
            &aml::Name::new(
                "_CRS".try_into().map_err(map_aml)?,
                &aml::ResourceTemplate::new(vec![
                    &aml::Interrupt::new(true, true, false, false, 4),
                    &aml::Io::new(0x3f8, 0x3f8, 1, 8),
                ]),
            )
            .map_err(map_aml)?,
        ],
    )
    .append_aml_bytes(aml)
    .map_err(map_aml)?;

    // PS2 (i8042) — required for reboot=k.
    aml::Device::new(
        "_SB_.PS2_".try_into().map_err(map_aml)?,
        vec![
            &aml::Name::new(
                "_HID".try_into().map_err(map_aml)?,
                &aml::EisaName::new("PNP0303").map_err(map_aml)?,
            )
            .map_err(map_aml)?,
            &aml::Method::new(
                "_STA".try_into().map_err(map_aml)?,
                0,
                false,
                vec![&aml::Return::new(&0x0fu8)],
            ),
            &aml::Name::new(
                "_CRS".try_into().map_err(map_aml)?,
                &aml::ResourceTemplate::new(vec![
                    &aml::Io::new(0x60, 0x60, 1, 1),
                    &aml::Io::new(0x64, 0x64, 1, 1),
                    &aml::Interrupt::new(true, true, false, false, 1),
                ]),
            )
            .map_err(map_aml)?,
        ],
    )
    .append_aml_bytes(aml)
    .map_err(map_aml)?;

    Ok(())
}

/// Virtio-mmio device AML entry (memory range + GSI interrupt in _CRS).
fn append_virtio_aml(aml: &mut Vec<u8>, mmio_addr: u64, mmio_size: u64, gsi: u32) -> Result<()> {
    use acpi_tables::aml::{self, AmlError};
    use acpi_tables::Aml;

    fn map_aml(e: AmlError) -> VmmError {
        VmmError::Memory(format!("AML encode: {e}"))
    }

    let dev_id = gsi - GSI_LEGACY_START;
    aml::Device::new(
        format!("V{dev_id:03}")
            .as_str()
            .try_into()
            .map_err(map_aml)?,
        vec![
            &aml::Name::new("_HID".try_into().map_err(map_aml)?, &"LNRO0005").map_err(map_aml)?,
            &aml::Name::new("_UID".try_into().map_err(map_aml)?, &dev_id).map_err(map_aml)?,
            &aml::Name::new("_CCA".try_into().map_err(map_aml)?, &aml::ONE).map_err(map_aml)?,
            &aml::Name::new(
                "_CRS".try_into().map_err(map_aml)?,
                &aml::ResourceTemplate::new(vec![
                    &aml::Memory32Fixed::new(
                        true,
                        mmio_addr
                            .try_into()
                            .map_err(|_| map_aml(AmlError::AddressRange))?,
                        mmio_size
                            .try_into()
                            .map_err(|_| map_aml(AmlError::AddressRange))?,
                    ),
                    &aml::Interrupt::new(true, true, false, false, gsi),
                ]),
            )
            .map_err(map_aml)?,
        ],
    )
    .append_aml_bytes(aml)
    .map_err(map_aml)?;

    Ok(())
}

// ============================================================
// Page table setup for 64-bit long mode
// ============================================================

fn setup_page_tables(mem: &GuestMemory) -> Result<()> {
    mem.inner
        .write_obj(PDPTE_ADDR | 0x03, GuestAddress(PML4_ADDR))
        .map_err(|e| VmmError::Memory(format!("write PML4: {e:?}")))?;
    mem.inner
        .write_obj(PDE_ADDR | 0x03, GuestAddress(PDPTE_ADDR))
        .map_err(|e| VmmError::Memory(format!("write PDPTE: {e:?}")))?;
    for i in 0..512u64 {
        let pde = (i << 21) | 0x83u64;
        mem.inner
            .write_obj(pde, GuestAddress(PDE_ADDR + i * 8))
            .map_err(|e| VmmError::Memory(format!("write PDE[{i}]: {e:?}")))?;
    }
    Ok(())
}

// ============================================================
// GDT helpers
// ============================================================

/// Build a kvm_segment from a GDT entry + table index.
fn kvm_segment_from_gdt(entry: u64, table_index: u8) -> kvm_segment {
    fn get_base(entry: u64) -> u64 {
        ((entry & 0xFF00000000000000) >> 32)
            | ((entry & 0x000000FF00000000) >> 16)
            | ((entry & 0x00000000FFFF0000) >> 16)
    }
    fn get_limit(entry: u64) -> u32 {
        let low = (entry & 0x0000_0000_0000_ffff) as u32;
        let high = ((entry & 0x000f_0000_0000_0000) >> 32) as u32;
        low | high
    }
    fn bits(entry: u64, shift: u32, mask: u64) -> u8 {
        ((entry >> shift) & mask) as u8
    }

    let present = bits(entry, 47, 1);
    kvm_segment {
        base: get_base(entry),
        limit: get_limit(entry),
        selector: (table_index * 8) as u16,
        type_: bits(entry, 40, 0xf),
        present,
        dpl: bits(entry, 45, 3),
        db: bits(entry, 54, 1),
        s: bits(entry, 44, 1),
        l: bits(entry, 53, 1),
        g: bits(entry, 55, 1),
        avl: bits(entry, 52, 1),
        unusable: if present == 0 { 1 } else { 0 },
        padding: 0,
    }
}

/// 32-bit flat segment.
fn flat_seg32(is_code: bool, selector: u16) -> kvm_segment {
    kvm_segment {
        base: 0,
        limit: 0xffffffff,
        selector,
        type_: if is_code { 0x0a } else { 0x02 },
        present: 1,
        dpl: 0,
        db: 1,
        s: 1,
        l: 0,
        g: 1,
        avl: 0,
        unusable: 0,
        padding: 0,
    }
}

// ============================================================
// Public API
// ============================================================

pub fn write_gdt(mem: &GuestMemory) -> Result<()> {
    for (i, &entry) in GDT32.iter().enumerate() {
        mem.inner
            .write_obj(entry, GuestAddress(GDT32_ADDR + i as u64 * 8))
            .map_err(|e| VmmError::Memory(format!("write GDT[{i}]: {e:?}")))?;
    }
    Ok(())
}

/// Fast boot (32-bit, no IRQCHIP) — used by benchmarks and unit tests.
pub fn setup_vcpu_for_bzimage_boot(vcpu: &VcpuFd, loaded: &LoadedKernel) -> Result<()> {
    setup_vcpu_for_bzimage_boot_full(vcpu, loaded, false, None)
}

/// Apply the boot CPUID to a vCPU with a specific local-APIC id.
///
/// Host CPUID passthrough (no normalization) with three
/// deltas on leaf 1: the initial APIC id (EBX[31:24]) is set to `apic_id`, the
/// APIC feature (EDX[9]) and TSC-Deadline (ECX[24]) are forced on, and x2APIC
/// (ECX[21]) is masked (it destabilizes nested virt). The BSP uses id 0; each
/// AP uses its vCPU id, so the guest's per-CPU APIC ids are distinct — required
/// for SMP (otherwise Linux sees every CPU with APIC id 0). Also used on SMP
/// restore to give each recreated AP its correct APIC id before its saved state.
pub(crate) fn apply_boot_cpuid_with_template(
    vcpu: &VcpuFd,
    apic_id: u8,
    template: &CpuTemplate,
) -> Result<()> {
    let kvm =
        kvm_ioctls::Kvm::new().map_err(|e| VmmError::Kvm(format!("Kvm::new for cpuid: {e}")))?;
    let mut cpuid = kvm
        .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
        .map_err(|e| VmmError::Kvm(format!("KVM_GET_SUPPORTED_CPUID: {e}")))?;
    normalize_boot_cpuid(&mut cpuid, apic_id);
    apply_cpuid_template(&mut cpuid, template);
    vcpu.set_cpuid2(&cpuid)
        .map_err(|e| VmmError::Kvm(format!("KVM_SET_CPUID2: {e}")))?;
    Ok(())
}

/// Configure an application processor (AP) vCPU for SMP boot.
///
/// Unlike the BSP, an AP gets NO boot registers. It starts in
/// `KVM_MP_STATE_UNINITIALIZED` and blocks in `KVM_RUN` until the guest BSP
/// sends INIT/SIPI; the in-kernel LAPIC turns that into a reset + CS:IP set from
/// the SIPI vector + a transition to RUNNABLE, and the guest kernel's trampoline
/// takes over. The VMM only sets the AP's CPUID (with its APIC id), CPU-template
/// MSRs, and MP state before starting its `KVM_RUN` loop. `apic_id` must equal
/// the vCPU id.
pub fn setup_ap_vcpu(vcpu: &VcpuFd, apic_id: u8) -> Result<()> {
    setup_ap_vcpu_with_template(vcpu, apic_id, &CpuTemplate::bare())
}

pub(crate) fn setup_ap_vcpu_with_template(
    vcpu: &VcpuFd,
    apic_id: u8,
    template: &CpuTemplate,
) -> Result<()> {
    apply_boot_cpuid_with_template(vcpu, apic_id, template)?;
    apply_msr_template(vcpu, template)?;
    let mp = kvm_bindings::kvm_mp_state {
        mp_state: kvm_bindings::KVM_MP_STATE_UNINITIALIZED,
    };
    vcpu.set_mp_state(mp)
        .map_err(|e| VmmError::Kvm(format!("KVM_SET_MP_STATE(AP {apic_id}): {e}")))?;
    Ok(())
}

/// Full vCPU setup.
/// - `full_boot=true` with ELF vmlinux: 64-bit long mode (page tables + EFER.LME).
/// - `full_boot=true` with bzImage: 32-bit protected mode (kernel does 32→64).
/// - `full_boot=false`: 32-bit protected mode (fast HLT path).
pub fn setup_vcpu_for_bzimage_boot_full(
    vcpu: &VcpuFd,
    loaded: &LoadedKernel,
    full_boot: bool,
    mem: Option<&GuestMemory>,
) -> Result<()> {
    setup_vcpu_for_bzimage_boot_full_with_template(
        vcpu,
        loaded,
        full_boot,
        mem,
        &CpuTemplate::bare(),
    )
}

pub(crate) fn setup_vcpu_for_bzimage_boot_full_with_template(
    vcpu: &VcpuFd,
    loaded: &LoadedKernel,
    full_boot: bool,
    mem: Option<&GuestMemory>,
    template: &CpuTemplate,
) -> Result<()> {
    // ELF vmlinux loads at 0x1000000+; bzImage loads at 0x100000.
    let is_elf = loaded.entry >= 0x1000000;

    // Set CPUID for ALL boot modes via the shared helper (APIC id 0 for the
    // BSP). Host CPUID passthrough with x2APIC masked + TSC-Deadline forced on;
    // see apply_boot_cpuid_with_template.
    apply_boot_cpuid_with_template(vcpu, 0, template)?;

    // Standard vCPU configure order:
    // CPUID → MSRs → REGS → FPU → SREGS → LAPIC

    if full_boot {
        // Set the boot MSRs (the proven 11-entry table; see
        // docs/DESIGN-CHOICES.md).
        let msr_data: [(u32, u64); 11] = [
            (0x00000174, 0),     // MSR_IA32_SYSENTER_CS
            (0x00000175, 0),     // MSR_IA32_SYSENTER_ESP
            (0x00000176, 0),     // MSR_IA32_SYSENTER_EIP
            (0xC0000081, 0),     // MSR_STAR
            (0xC0000082, 0),     // MSR_LSTAR
            (0xC0000083, 0),     // MSR_CSTAR
            (0xC0000084, 0),     // MSR_SYSCALL_MASK
            (0xC0000102, 0),     // MSR_KERNEL_GS_BASE
            (0x00000010, 0),     // MSR_IA32_TSC
            (0x000001a0, 1),     // MSR_IA32_MISC_ENABLE = FAST_STRING
            (0x000002ff, 0x806), // MSR_MTRRdefType = (1<<11)|6 write-back
        ];
        let msr_entries: Vec<kvm_bindings::kvm_msr_entry> = msr_data
            .iter()
            .map(|&(index, data)| kvm_bindings::kvm_msr_entry {
                index,
                data,
                ..Default::default()
            })
            .collect();
        let msrs = kvm_bindings::Msrs::from_entries(&msr_entries)
            .map_err(|e| VmmError::Kvm(format!("Msrs::from_entries: {e:?}")))?;
        set_msrs_checked(vcpu, &msrs, "KVM_SET_MSRS(boot)")?;
    }
    apply_msr_template(vcpu, template)?;

    // Set REGS.
    let mut regs = vcpu
        .get_regs()
        .map_err(|e| VmmError::Kvm(format!("KVM_GET_REGS: {e}")))?;
    regs.rip = loaded.entry;
    regs.rsi = loaded.zero_page_addr;
    regs.rsp = 0x8ff0;
    regs.rbp = 0x8ff0;
    regs.rflags = 0x2;
    vcpu.set_regs(&regs)
        .map_err(|e| VmmError::Kvm(format!("KVM_SET_REGS: {e}")))?;

    // Set FPU.
    if full_boot {
        let fpu = kvm_bindings::kvm_fpu {
            fpr: [[0; 16]; 8],
            fcw: 0x037f,
            fsw: 0,
            ftwx: 0,
            pad1: 0,
            last_opcode: 0,
            last_ip: 0,
            last_dp: 0,
            xmm: [[0; 16]; 16],
            mxcsr: 0x1f80,
            pad2: 0,
        };
        vcpu.set_fpu(&fpu)
            .map_err(|e| VmmError::Kvm(format!("KVM_SET_FPU: {e}")))?;
    }

    // Set SREGS.
    let mut sregs = vcpu
        .get_sregs()
        .map_err(|e| VmmError::Kvm(format!("KVM_GET_SREGS: {e}")))?;

    if full_boot && is_elf {
        let Some(mem) = mem else {
            return Err(VmmError::Memory(
                "full ELF boot setup requires guest memory".into(),
            ));
        };
        for (i, &entry) in BOOT_GDT.iter().enumerate() {
            mem.inner
                .write_obj(entry, GuestAddress(BOOT_GDT_ADDR + i as u64 * 8))
                .map_err(|e| VmmError::Memory(format!("write GDT64[{i}]: {e:?}")))?;
        }
        mem.inner
            .write_obj(0u64, GuestAddress(BOOT_IDT_ADDR))
            .map_err(|e| VmmError::Memory(format!("write IDT: {e:?}")))?;
        sregs.gdt.base = BOOT_GDT_ADDR;
        sregs.gdt.limit = (BOOT_GDT.len() * 8) as u16 - 1;
        sregs.idt.base = BOOT_IDT_ADDR;
        sregs.idt.limit = 7;
        sregs.cs = kvm_segment_from_gdt(BOOT_GDT[1], 1);
        sregs.ds = kvm_segment_from_gdt(BOOT_GDT[2], 2);
        sregs.es = sregs.ds;
        sregs.fs = sregs.ds;
        sregs.gs = sregs.ds;
        sregs.ss = sregs.ds;
        sregs.tr = kvm_segment_from_gdt(BOOT_GDT[3], 3);
        setup_page_tables(mem)?;
        sregs.cr3 = PML4_ADDR;
        sregs.cr4 |= CR4_PAE;
        sregs.efer |= EFER_LME | EFER_LMA;
        sregs.cr0 |= CR0_PE | CR0_PG;
    } else {
        if let Some(m) = mem {
            write_gdt(m)?;
        }
        sregs.gdt.base = GDT32_ADDR;
        sregs.gdt.limit = (GDT32.len() * 8) as u16 - 1;
        sregs.cs = flat_seg32(true, 0x08);
        sregs.ds = flat_seg32(false, 0x10);
        sregs.es = sregs.ds;
        sregs.ss = sregs.ds;
        sregs.fs = sregs.ds;
        sregs.gs = sregs.ds;
        sregs.cr0 |= CR0_PE | CR0_MP | CR0_ET | CR0_NE;
        sregs.cr4 = 0;
        sregs.efer = 0;
    }

    vcpu.set_sregs(&sregs)
        .map_err(|e| VmmError::Kvm(format!("KVM_SET_SREGS: {e}")))?;
    log::info!(
        "sregs: cr0=0x{:x} efer=0x{:x} cr3=0x{:x} cr4=0x{:x} apic_base=0x{:x}",
        sregs.cr0,
        sregs.efer,
        sregs.cr3,
        sregs.cr4,
        sregs.apic_base
    );

    // Set LAPIC LINT0=EXTINT, LVT1=NMI.
    if full_boot {
        set_lint(vcpu).map_err(|e| {
            log::error!("set_lint failed: {e}");
            e
        })?;
        log::info!("set_lint: LVT0=EXTINT, LVT1=NMI (ok)");
    }

    Ok(())
}

#[allow(unused_variables)]
pub fn setup_vcpu_for_kernel_boot(
    vcpu: &VcpuFd,
    loaded: &LoadedKernel,
    mem: &GuestMemory,
    mem_size: u64,
) -> Result<()> {
    Err(VmmError::Kvm(
        "ELF vmlinux boot path not implemented; use bzImage".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dump DSDT bytes for manual verification against iasl reference.
    /// Run with: cargo test -p vmm-core --features kvm dump_dsdt_bytes -- --nocapture
    #[test]
    fn dump_dsdt_bytes() {
        let dsdt = build_dsdt(&[]).unwrap();
        eprintln!("\nDSDT (0 devices) — {} bytes:", dsdt.len());
        for (i, b) in dsdt.iter().enumerate() {
            if i % 16 == 0 {
                eprint!("\n  {:04x}: ", i);
            }
            eprint!("{:02x} ", b);
        }
        eprintln!();
    }

    /// Legacy COM1 AML matches the expected reference bytes (includes _DDN).
    #[test]
    fn com1_aml_matches_reference() {
        use acpi_tables::aml::{self, Aml};

        let mut legacy = Vec::new();
        append_legacy_aml(&mut legacy).unwrap();
        let mut com1_only = Vec::new();
        aml::Device::new(
            "_SB_.COM1".try_into().unwrap(),
            vec![
                &aml::Name::new(
                    "_HID".try_into().unwrap(),
                    &aml::EisaName::new("PNP0501").unwrap(),
                )
                .unwrap(),
                &aml::Name::new("_UID".try_into().unwrap(), &0u8).unwrap(),
                &aml::Name::new("_DDN".try_into().unwrap(), &"COM1").unwrap(),
                &aml::Name::new(
                    "_CRS".try_into().unwrap(),
                    &aml::ResourceTemplate::new(vec![
                        &aml::Interrupt::new(true, true, false, false, 4),
                        &aml::Io::new(0x3f8, 0x3f8, 1, 8),
                    ]),
                )
                .unwrap(),
            ],
        )
        .append_aml_bytes(&mut com1_only)
        .unwrap();
        assert_eq!(
            &legacy[..com1_only.len()],
            com1_only.as_slice(),
            "COM1 AML prefix mismatch"
        );
    }
}

// ============================================================
// Full vCPU state snapshot/restore (for faithful resume)
// ============================================================

/// Complete vCPU state captured at snapshot time and re-applied on restore so a
/// restored VM resumes execution exactly where it paused. Uses the raw KVM
/// structs (serde-enabled via kvm-bindings) plus an explicit MSR list.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct VcpuFullState {
    pub regs: kvm_bindings::kvm_regs,
    pub sregs: kvm_bindings::kvm_sregs,
    pub xsave: kvm_bindings::kvm_xsave,
    pub xcrs: kvm_bindings::kvm_xcrs,
    pub lapic: kvm_bindings::kvm_lapic_state,
    pub mp_state: kvm_bindings::kvm_mp_state,
    pub vcpu_events: kvm_bindings::kvm_vcpu_events,
    pub msrs: Vec<(u32, u64)>,
}

/// MSRs we save/restore: TSC, syscall/segment bases, sysenter, misc-enable and
/// TSC-deadline — the set that matters for a correct x86_64 resume.
const SNAPSHOT_MSRS: &[u32] = &[
    0x0000_0010, // IA32_TSC
    0x0000_0174, // IA32_SYSENTER_CS
    0x0000_0175, // IA32_SYSENTER_ESP
    0x0000_0176, // IA32_SYSENTER_EIP
    0x0000_01a0, // IA32_MISC_ENABLE
    0x0000_06e0, // IA32_TSC_DEADLINE
    0xc000_0080, // IA32_EFER
    0xc000_0081, // STAR
    0xc000_0082, // LSTAR
    0xc000_0083, // CSTAR
    0xc000_0084, // SFMASK
    0xc000_0100, // FS_BASE
    0xc000_0101, // GS_BASE
    0xc000_0102, // KERNEL_GS_BASE
];

/// Capture the full vCPU state. Call with the vCPU stopped (paused).
pub fn capture_vcpu_full_state(vcpu: &VcpuFd) -> Result<VcpuFullState> {
    let entries: Vec<kvm_bindings::kvm_msr_entry> = SNAPSHOT_MSRS
        .iter()
        .map(|&index| kvm_bindings::kvm_msr_entry {
            index,
            ..Default::default()
        })
        .collect();
    let mut msrs = kvm_bindings::Msrs::from_entries(&entries)
        .map_err(|e| VmmError::Kvm(format!("Msrs::from_entries: {e:?}")))?;
    get_msrs_checked(vcpu, &mut msrs, "KVM_GET_MSRS(snapshot)")?;
    let msr_vals: Vec<(u32, u64)> = msrs.as_slice().iter().map(|e| (e.index, e.data)).collect();

    Ok(VcpuFullState {
        regs: vcpu
            .get_regs()
            .map_err(|e| VmmError::Kvm(format!("KVM_GET_REGS: {e}")))?,
        sregs: vcpu
            .get_sregs()
            .map_err(|e| VmmError::Kvm(format!("KVM_GET_SREGS: {e}")))?,
        xsave: vcpu
            .get_xsave()
            .map_err(|e| VmmError::Kvm(format!("KVM_GET_XSAVE: {e}")))?,
        xcrs: vcpu
            .get_xcrs()
            .map_err(|e| VmmError::Kvm(format!("KVM_GET_XCRS: {e}")))?,
        lapic: vcpu
            .get_lapic()
            .map_err(|e| VmmError::Kvm(format!("KVM_GET_LAPIC: {e}")))?,
        mp_state: vcpu
            .get_mp_state()
            .map_err(|e| VmmError::Kvm(format!("KVM_GET_MP_STATE: {e}")))?,
        vcpu_events: vcpu
            .get_vcpu_events()
            .map_err(|e| VmmError::Kvm(format!("KVM_GET_VCPU_EVENTS: {e}")))?,
        msrs: msr_vals,
    })
}

/// Re-apply a captured vCPU state. Call after KVM_SET_CPUID2 on a fresh vCPU.
pub fn restore_vcpu_full_state(vcpu: &VcpuFd, s: &VcpuFullState) -> Result<()> {
    vcpu.set_sregs(&s.sregs)
        .map_err(|e| VmmError::Kvm(format!("KVM_SET_SREGS: {e}")))?;
    vcpu.set_regs(&s.regs)
        .map_err(|e| VmmError::Kvm(format!("KVM_SET_REGS: {e}")))?;
    vcpu.set_xsave(&s.xsave)
        .map_err(|e| VmmError::Kvm(format!("KVM_SET_XSAVE: {e}")))?;
    vcpu.set_xcrs(&s.xcrs)
        .map_err(|e| VmmError::Kvm(format!("KVM_SET_XCRS: {e}")))?;
    // Set IA32_TSC before IA32_TSC_DEADLINE (KVM primes the deadline off the
    // current TSC), hence the deferred-MSR ordering below.
    let mut ordered = s.msrs.clone();
    ordered.sort_by_key(|&(index, _)| u8::from(index == 0x0000_06e0));
    let entries: Vec<kvm_bindings::kvm_msr_entry> = ordered
        .iter()
        .map(|&(index, data)| kvm_bindings::kvm_msr_entry {
            index,
            data,
            ..Default::default()
        })
        .collect();
    let msrs = kvm_bindings::Msrs::from_entries(&entries)
        .map_err(|e| VmmError::Kvm(format!("Msrs::from_entries: {e:?}")))?;
    set_msrs_checked(vcpu, &msrs, "KVM_SET_MSRS(restore)")?;
    vcpu.set_lapic(&s.lapic)
        .map_err(|e| VmmError::Kvm(format!("KVM_SET_LAPIC: {e}")))?;
    vcpu.set_vcpu_events(&s.vcpu_events)
        .map_err(|e| VmmError::Kvm(format!("KVM_SET_VCPU_EVENTS: {e}")))?;
    vcpu.set_mp_state(s.mp_state)
        .map_err(|e| VmmError::Kvm(format!("KVM_SET_MP_STATE: {e}")))?;
    Ok(())
}
