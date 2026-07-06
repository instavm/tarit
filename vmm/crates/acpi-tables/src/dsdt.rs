// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::mem::size_of;

use vm_memory::{Address, Bytes, GuestAddress, GuestMemory};
use zerocopy::IntoBytes;

use crate::{checksum, AcpiError, Result, Sdt, SdtHeader};

/// Differentiated System Description Table (DSDT)
///
/// Table that includes hardware definition blocks.
/// More information about this table can be found in the ACPI specification:
/// https://uefi.org/specs/ACPI/6.5/05_ACPI_Software_Programming_Model.html#differentiated-system-description-table-dsdt
#[derive(Debug, Clone)]
pub struct Dsdt {
    header: SdtHeader,
    definition_block: Vec<u8>,
}

impl Dsdt {
    pub fn try_new(
        oem_id: [u8; 6],
        oem_table_id: [u8; 8],
        oem_revision: u32,
        definition_block: Vec<u8>,
    ) -> Result<Self> {
        let length = checked_table_length(size_of::<SdtHeader>(), definition_block.len())?;
        Ok(Self::new_with_length(
            oem_id,
            oem_table_id,
            oem_revision,
            definition_block,
            length,
        ))
    }

    pub fn new(
        oem_id: [u8; 6],
        oem_table_id: [u8; 8],
        oem_revision: u32,
        definition_block: Vec<u8>,
    ) -> Self {
        let length = table_length_or_max(size_of::<SdtHeader>(), definition_block.len());
        Self::new_with_length(oem_id, oem_table_id, oem_revision, definition_block, length)
    }

    fn new_with_length(
        oem_id: [u8; 6],
        oem_table_id: [u8; 8],
        oem_revision: u32,
        definition_block: Vec<u8>,
        length: u32,
    ) -> Self {
        let header = SdtHeader::new(*b"DSDT", length, 2, oem_id, oem_table_id, oem_revision);

        let mut dsdt = Dsdt {
            header,
            definition_block,
        };

        dsdt.header.checksum =
            checksum(&[dsdt.header.as_bytes(), dsdt.definition_block.as_slice()]);
        dsdt
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

impl Sdt for Dsdt {
    fn len(&self) -> usize {
        self.header.length.get() as usize
    }

    fn write_to_guest<AS: GuestMemory>(&mut self, mem: &AS, address: GuestAddress) -> Result<()> {
        mem.write_slice(self.header.as_bytes(), address)?;
        let address = address
            .checked_add(size_of::<SdtHeader>() as u64)
            .ok_or(AcpiError::InvalidGuestAddress)?;
        mem.write_slice(self.definition_block.as_slice(), address)?;

        Ok(())
    }
}
