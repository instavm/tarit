// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// Copyright 2023 Rivos, Inc.
//
// SPDX-License-Identifier: Apache-2.0

use std::mem::size_of;

use vm_memory::{Address, Bytes, GuestAddress, GuestMemory};
use zerocopy::IntoBytes;

use crate::{checksum, AcpiError, Result, Sdt, SdtHeader};

/// Extended System Description Table (XSDT)
///
/// This table provides 64bit addresses to the rest of the ACPI tables defined by the platform
/// More information about this table can be found in the ACPI specification:
/// https://uefi.org/specs/ACPI/6.5/05_ACPI_Software_Programming_Model.html#extended-system-description-table-xsdt
#[derive(Clone, Default, Debug)]
pub struct Xsdt {
    header: SdtHeader,
    tables: Vec<u8>,
}

impl Xsdt {
    pub fn try_new(
        oem_id: [u8; 6],
        oem_table_id: [u8; 8],
        oem_revision: u32,
        tables: Vec<u64>,
    ) -> Result<Self> {
        let tables_bytes = tables_to_bytes(tables);
        let length = checked_table_length(size_of::<SdtHeader>(), tables_bytes.len())?;
        Ok(Self::new_with_length(
            oem_id,
            oem_table_id,
            oem_revision,
            tables_bytes,
            length,
        ))
    }

    pub fn new(
        oem_id: [u8; 6],
        oem_table_id: [u8; 8],
        oem_revision: u32,
        tables: Vec<u64>,
    ) -> Self {
        let tables_bytes = tables_to_bytes(tables);
        let length = table_length_or_max(size_of::<SdtHeader>(), tables_bytes.len());
        Self::new_with_length(oem_id, oem_table_id, oem_revision, tables_bytes, length)
    }

    fn new_with_length(
        oem_id: [u8; 6],
        oem_table_id: [u8; 8],
        oem_revision: u32,
        tables_bytes: Vec<u8>,
        length: u32,
    ) -> Self {
        let header = SdtHeader::new(*b"XSDT", length, 1, oem_id, oem_table_id, oem_revision);

        let mut xsdt = Xsdt {
            header,

            tables: tables_bytes,
        };

        xsdt.header.checksum = checksum(&[xsdt.header.as_bytes(), (xsdt.tables.as_slice())]);

        xsdt
    }
}

fn tables_to_bytes(tables: Vec<u64>) -> Vec<u8> {
    let mut tables_bytes = Vec::with_capacity(8usize.saturating_mul(tables.len()));
    for addr in tables {
        tables_bytes.extend(&addr.to_le_bytes());
    }
    tables_bytes
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

impl Sdt for Xsdt {
    fn len(&self) -> usize {
        std::mem::size_of::<SdtHeader>() + self.tables.len()
    }

    fn write_to_guest<M: GuestMemory>(&mut self, mem: &M, address: GuestAddress) -> Result<()> {
        mem.write_slice(self.header.as_bytes(), address)?;
        let address = address
            .checked_add(size_of::<SdtHeader>() as u64)
            .ok_or(AcpiError::InvalidGuestAddress)?;
        mem.write_slice(self.tables.as_slice(), address)?;
        Ok(())
    }
}
