// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// Copyright 2023 Rivos, Inc.
//
// SPDX-License-Identifier: Apache-2.0

use std::mem::size_of;

use vm_memory::{Address, Bytes, GuestAddress, GuestMemory};
use zerocopy::little_endian::U32;
use zerocopy::{Immutable, IntoBytes};

use crate::{checksum, AcpiError, Result, Sdt, SdtHeader};

const MADT_CPU_ENABLE_FLAG: u32 = 0;

// clippy doesn't understand that we actually "use" the fields of this struct when we serialize
// them as bytes in guest memory, so here we just ignore dead code to avoid having to name
// everything with an underscore prefix
#[allow(dead_code)]
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, Default, IntoBytes, Immutable)]
pub struct LocalAPIC {
    r#type: u8,
    length: u8,
    processor_uid: u8,
    apic_id: u8,
    flags: U32,
}

impl LocalAPIC {
    pub fn new(cpu_id: u8) -> Self {
        Self {
            r#type: 0,
            length: 8,
            processor_uid: cpu_id,
            apic_id: cpu_id,
            flags: U32::new(1u32 << MADT_CPU_ENABLE_FLAG),
        }
    }
}

// clippy doesn't understand that we actually "use" the fields of this struct when we serialize
// them as bytes in guest memory, so here we just ignore dead code to avoid having to name
// everything with an underscore prefix
#[allow(dead_code)]
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, Default, IntoBytes, Immutable)]
pub struct IoAPIC {
    r#type: u8,
    length: u8,
    ioapic_id: u8,
    reserved: u8,
    apic_address: U32,
    gsi_base: U32,
}

impl IoAPIC {
    pub fn new(ioapic_id: u8, apic_address: u32) -> Self {
        IoAPIC {
            r#type: 1,
            length: 12,
            ioapic_id,
            reserved: 0,
            apic_address: U32::new(apic_address),
            gsi_base: U32::ZERO,
        }
    }
}

// clippy doesn't understand that we actually "use" the fields of this struct when we serialize
// them as bytes in guest memory, so here we just ignore dead code to avoid having to name
// everything with an underscore prefix
#[allow(dead_code)]
#[repr(C, packed)]
#[derive(Debug, IntoBytes, Immutable)]
struct MadtHeader {
    sdt: SdtHeader,
    base_address: U32,
    flags: U32,
}

/// Multiple APIC Description Table (MADT)
///
/// This table includes information about the interrupt controllers of the device.
/// More information about this table can be found in the ACPI specification:
/// https://uefi.org/specs/ACPI/6.5/05_ACPI_Software_Programming_Model.html#multiple-apic-description-table-madt
#[derive(Debug)]
pub struct Madt {
    header: MadtHeader,
    interrupt_controllers: Vec<u8>,
}

impl Madt {
    pub fn try_new(
        oem_id: [u8; 6],
        oem_table_id: [u8; 8],
        oem_revision: u32,
        base_address: u32,
        interrupt_controllers: Vec<u8>,
    ) -> Result<Self> {
        let length = checked_table_length(size_of::<MadtHeader>(), interrupt_controllers.len())?;
        Ok(Self::new_with_length(
            oem_id,
            oem_table_id,
            oem_revision,
            base_address,
            interrupt_controllers,
            length,
        ))
    }

    pub fn new(
        oem_id: [u8; 6],
        oem_table_id: [u8; 8],
        oem_revision: u32,
        base_address: u32,
        interrupt_controllers: Vec<u8>,
    ) -> Self {
        let length = table_length_or_max(size_of::<MadtHeader>(), interrupt_controllers.len());
        Self::new_with_length(
            oem_id,
            oem_table_id,
            oem_revision,
            base_address,
            interrupt_controllers,
            length,
        )
    }

    fn new_with_length(
        oem_id: [u8; 6],
        oem_table_id: [u8; 8],
        oem_revision: u32,
        base_address: u32,
        interrupt_controllers: Vec<u8>,
        length: u32,
    ) -> Self {
        let sdt_header = SdtHeader::new(*b"APIC", length, 6, oem_id, oem_table_id, oem_revision);

        let mut header = MadtHeader {
            sdt: sdt_header,
            base_address: U32::new(base_address),
            flags: U32::ZERO,
        };

        header.sdt.checksum = checksum(&[header.as_bytes(), interrupt_controllers.as_bytes()]);

        Madt {
            header,
            interrupt_controllers,
        }
    }
}

fn checked_table_length(header_len: usize, payload_len: usize) -> Result<u32> {
    let length = header_len
        .checked_add(payload_len)
        .ok_or(AcpiError::TableLength {
            length: usize::MAX,
            max: u32::MAX,
        })?;
    u32::try_from(length).map_err(|_| AcpiError::TableLength {
        length,
        max: u32::MAX,
    })
}

fn table_length_or_max(header_len: usize, payload_len: usize) -> u32 {
    checked_table_length(header_len, payload_len).unwrap_or(u32::MAX)
}

impl Sdt for Madt {
    fn len(&self) -> usize {
        self.header.sdt.length.get() as usize
    }

    fn write_to_guest<M: GuestMemory>(&mut self, mem: &M, address: GuestAddress) -> Result<()> {
        mem.write_slice(self.header.as_bytes(), address)?;
        let address = address
            .checked_add(size_of::<MadtHeader>() as u64)
            .ok_or(AcpiError::InvalidGuestAddress)?;
        mem.write_slice(self.interrupt_controllers.as_bytes(), address)?;

        Ok(())
    }
}
