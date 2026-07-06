//! Real virtio-blk backend — services virtqueue requests against a backing
//! file via pread/pwrite.

use std::cmp;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use thiserror::Error;

use crate::virtio::blk::{req_type, status, validate_req, BlkReqHeader};

const SECTOR_SIZE: u64 = 512;
const SECTOR_SIZE_USIZE: usize = SECTOR_SIZE as usize;
const VIRTIO_BLK_T_BARRIER: u32 = 0x8000_0000;

/// Copy-on-write overlay layout:
///
/// ```text
/// 0x0000..0x1000  4096-byte header
///                 magic[8] = "VMMCOW1\0"
///                 version u32 = 1
///                 block_size u32 = 512
///                 base_len u64 = usable base bytes
///                 blocks u64 = base_len / 512
///                 bitmap_offset u64
///                 bitmap_len u64
///                 data_offset u64
/// header..data    bitmap, one bit per 512-byte sector
/// data_offset..   sparse overlay data, block N at data_offset + N * 512
/// ```
///
/// A dirty bitmap bit means reads for that sector come from the overlay.
/// Clear bits fall through to the read-only base image. Partial first writes
/// copy the base sector into the overlay before applying the guest bytes.
const COW_MAGIC: [u8; 8] = *b"VMMCOW1\0";
const COW_VERSION: u32 = 1;
const COW_HEADER_LEN: usize = 4096;

#[derive(Debug, Error)]
pub enum BlkBackendError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("validation: {0}")]
    Validation(String),
    #[error("backing file not open")]
    NotOpen,
}

#[derive(Debug, Clone, Copy)]
struct CowHeader {
    base_len: u64,
    blocks: u64,
    bitmap_offset: u64,
    bitmap_len: u64,
    data_offset: u64,
}

impl CowHeader {
    fn new(blocks: u64) -> Result<Self, BlkBackendError> {
        let bitmap_len = bitmap_len(blocks)?;
        let header_len = COW_HEADER_LEN as u64;
        let data_offset = align_up(
            header_len
                .checked_add(bitmap_len)
                .ok_or_else(|| BlkBackendError::Validation("overlay header overflows".into()))?,
            SECTOR_SIZE,
        )?;
        Ok(Self {
            base_len: blocks.checked_mul(SECTOR_SIZE).ok_or_else(|| {
                BlkBackendError::Validation("overlay base length overflows".into())
            })?,
            blocks,
            bitmap_offset: header_len,
            bitmap_len,
            data_offset,
        })
    }

    fn encode(self) -> [u8; COW_HEADER_LEN] {
        let mut buf = [0u8; COW_HEADER_LEN];
        buf[0..8].copy_from_slice(&COW_MAGIC);
        buf[8..12].copy_from_slice(&COW_VERSION.to_le_bytes());
        buf[12..16].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
        buf[16..24].copy_from_slice(&self.base_len.to_le_bytes());
        buf[24..32].copy_from_slice(&self.blocks.to_le_bytes());
        buf[32..40].copy_from_slice(&self.bitmap_offset.to_le_bytes());
        buf[40..48].copy_from_slice(&self.bitmap_len.to_le_bytes());
        buf[48..56].copy_from_slice(&self.data_offset.to_le_bytes());
        buf
    }

    fn decode(buf: &[u8]) -> Result<Self, BlkBackendError> {
        if buf.len() < COW_HEADER_LEN {
            return Err(BlkBackendError::Validation(
                "overlay header is truncated".into(),
            ));
        }
        if buf[0..8] != COW_MAGIC {
            return Err(BlkBackendError::Validation("overlay magic mismatch".into()));
        }
        let version = read_u32(buf, 8)?;
        if version != COW_VERSION {
            return Err(BlkBackendError::Validation(format!(
                "unsupported overlay version {version}"
            )));
        }
        let block_size = read_u32(buf, 12)?;
        if block_size != SECTOR_SIZE as u32 {
            return Err(BlkBackendError::Validation(format!(
                "unsupported overlay block size {block_size}"
            )));
        }
        Ok(Self {
            base_len: read_u64(buf, 16)?,
            blocks: read_u64(buf, 24)?,
            bitmap_offset: read_u64(buf, 32)?,
            bitmap_len: read_u64(buf, 40)?,
            data_offset: read_u64(buf, 48)?,
        })
    }
}

struct CowOverlay {
    base: File,
    overlay: File,
    bitmap: Vec<u8>,
    header: CowHeader,
}

impl CowOverlay {
    fn open(base: File, mut overlay: File, blocks: u64) -> Result<Self, BlkBackendError> {
        let expected = CowHeader::new(blocks)?;
        let overlay_len = overlay.metadata()?.len();
        let header = if overlay_len == 0 {
            let header_buf = expected.encode();
            write_all_at(&mut overlay, 0, &header_buf)?;
            let bitmap = vec![0u8; usize_from_u64(expected.bitmap_len, "bitmap length")?];
            write_all_at(&mut overlay, expected.bitmap_offset, &bitmap)?;
            overlay.set_len(expected.data_offset)?;
            overlay.sync_all()?;
            expected
        } else {
            if overlay_len < COW_HEADER_LEN as u64 {
                return Err(BlkBackendError::Validation(
                    "overlay file is too small for header".into(),
                ));
            }
            let mut header_buf = [0u8; COW_HEADER_LEN];
            read_exact_at(&mut overlay, 0, &mut header_buf)?;
            CowHeader::decode(&header_buf)?
        };
        validate_overlay_header(header, expected)?;

        let mut bitmap = vec![0u8; usize_from_u64(header.bitmap_len, "bitmap length")?];
        read_exact_at(&mut overlay, header.bitmap_offset, &mut bitmap)?;
        validate_unused_bitmap_bits(&bitmap, header.blocks)?;

        Ok(Self {
            base,
            overlay,
            bitmap,
            header,
        })
    }

    fn read_at(&mut self, offset: u64, data: &mut [u8]) -> Result<(), BlkBackendError> {
        for segment in BlockSegments::new(offset, data.len()) {
            let dst = &mut data[segment.buffer_range()];
            let source_offset = if self.is_dirty(segment.block) {
                self.overlay_block_offset(segment.block)?
                    .checked_add(segment.within_block as u64)
                    .ok_or_else(|| BlkBackendError::Validation("overlay read overflows".into()))?
            } else {
                segment
                    .absolute_offset()
                    .ok_or_else(|| BlkBackendError::Validation("base read overflows".into()))?
            };
            if self.is_dirty(segment.block) {
                read_exact_at(&mut self.overlay, source_offset, dst)?;
            } else {
                read_exact_at(&mut self.base, source_offset, dst)?;
            }
        }
        Ok(())
    }

    fn write_at(
        &mut self,
        offset: u64,
        data: &[u8],
        force_unit_access: bool,
    ) -> Result<(), BlkBackendError> {
        for segment in BlockSegments::new(offset, data.len()) {
            let dirty = self.is_dirty(segment.block);
            let dst_offset = self
                .overlay_block_offset(segment.block)?
                .checked_add(segment.within_block as u64)
                .ok_or_else(|| BlkBackendError::Validation("overlay write overflows".into()))?;

            if !dirty && (segment.within_block != 0 || segment.len != SECTOR_SIZE_USIZE) {
                self.copy_base_block_to_overlay(segment.block)?;
            }

            write_all_at(&mut self.overlay, dst_offset, &data[segment.buffer_range()])?;
            if !dirty {
                self.mark_dirty(segment.block)?;
            }
        }

        if force_unit_access {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BlkBackendError> {
        self.overlay.sync_all()?;
        Ok(())
    }

    fn fd(&self) -> std::os::fd::RawFd {
        self.overlay.as_raw_fd()
    }

    fn is_dirty(&self, block: u64) -> bool {
        let byte_idx = (block / 8) as usize;
        let bit = 1u8 << (block % 8);
        self.bitmap[byte_idx] & bit != 0
    }

    fn mark_dirty(&mut self, block: u64) -> Result<(), BlkBackendError> {
        let byte_idx = (block / 8) as usize;
        let bit = 1u8 << (block % 8);
        self.bitmap[byte_idx] |= bit;
        let bitmap_offset = self
            .header
            .bitmap_offset
            .checked_add(byte_idx as u64)
            .ok_or_else(|| BlkBackendError::Validation("bitmap offset overflows".into()))?;
        write_all_at(
            &mut self.overlay,
            bitmap_offset,
            std::slice::from_ref(&self.bitmap[byte_idx]),
        )?;
        Ok(())
    }

    fn copy_base_block_to_overlay(&mut self, block: u64) -> Result<(), BlkBackendError> {
        let mut buf = [0u8; SECTOR_SIZE_USIZE];
        let base_offset = block
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| BlkBackendError::Validation("base block offset overflows".into()))?;
        let overlay_offset = self.overlay_block_offset(block)?;
        read_exact_at(&mut self.base, base_offset, &mut buf)?;
        write_all_at(&mut self.overlay, overlay_offset, &buf)?;
        Ok(())
    }

    fn overlay_block_offset(&self, block: u64) -> Result<u64, BlkBackendError> {
        self.header
            .data_offset
            .checked_add(block.checked_mul(SECTOR_SIZE).ok_or_else(|| {
                BlkBackendError::Validation("overlay block offset overflows".into())
            })?)
            .ok_or_else(|| BlkBackendError::Validation("overlay block offset overflows".into()))
    }
}

enum BackendStorage {
    Raw(File),
    Cow(CowOverlay),
}

/// A file-backed virtio-blk backend. Services read/write/flush requests
/// against a host file or a copy-on-write overlay over a read-only base.
pub struct BlkBackend {
    storage: BackendStorage,
    pub read_only: bool,
    pub sectors: u64,
}

impl BlkBackend {
    /// Open a backing file for the block device.
    pub fn open(path: &PathBuf, read_only: bool) -> Result<Self, BlkBackendError> {
        let file = if read_only {
            File::open(path)?
        } else {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?
        };
        let metadata = file.metadata()?;
        let sectors = metadata.len() / 512;
        log::info!(
            "blk backend: {} ({} sectors, read_only={read_only})",
            path.display(),
            sectors
        );
        Ok(Self {
            storage: BackendStorage::Raw(file),
            read_only,
            sectors,
        })
    }

    /// Open a read-only base image with a private sparse copy-on-write overlay.
    pub fn open_cow(base_path: &PathBuf, overlay_path: &PathBuf) -> Result<Self, BlkBackendError> {
        let base = File::open(base_path)?;
        let metadata = base.metadata()?;
        let sectors = metadata.len() / SECTOR_SIZE;
        let overlay = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(overlay_path)?;
        let cow = CowOverlay::open(base, overlay, sectors)?;
        log::info!(
            "blk backend cow: base={} overlay={} ({} sectors)",
            base_path.display(),
            overlay_path.display(),
            sectors
        );
        Ok(Self {
            storage: BackendStorage::Cow(cow),
            read_only: false,
            sectors,
        })
    }

    /// Service a single block request.
    ///
    /// - `header`: the parsed virtio_blk_req (type, sector)
    /// - `data`: the data buffer (read: device fills this; write: guest provides this)
    ///
    /// Returns the status byte to write back to the guest's status descriptor.
    pub fn service(&mut self, header: &BlkReqHeader, data: &mut [u8]) -> u8 {
        // Validate the request.
        let (op, offset, force_unit_access) = match self.validate_service_req(header, data.len()) {
            Ok(validated) => validated,
            Err(e) => {
                log::warn!("blk: validation failed: {e}");
                return status::IO_ERR;
            }
        };

        match op {
            req_type::IN => {
                if let Err(e) = self.read_at(offset, data) {
                    log::warn!("blk: read failed: {e}");
                    return status::IO_ERR;
                }
                status::OK
            }
            req_type::OUT => {
                if self.read_only {
                    log::warn!("blk: write to read-only device");
                    return status::IO_ERR;
                }
                if let Err(e) = self.write_at(offset, data, force_unit_access) {
                    log::warn!("blk: write failed: {e}");
                    return status::IO_ERR;
                }
                status::OK
            }
            req_type::FLUSH => {
                if let Err(e) = self.flush() {
                    log::warn!("blk: flush failed: {e}");
                    return status::IO_ERR;
                }
                status::OK
            }
            req_type::GET_ID => {
                // Return a 20-byte serial string (virtio 1.x §5.2.5).
                let serial = b"vmm-blk\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
                let len = data.len().min(20);
                data[..len].copy_from_slice(&serial[..len]);
                status::OK
            }
            _ => {
                log::warn!("blk: unsupported req_type {}", header.req_type);
                status::UNSUPP
            }
        }
    }

    /// Get the raw fd (for epoll-based I/O).
    pub fn fd(&self) -> std::os::fd::RawFd {
        match &self.storage {
            BackendStorage::Raw(file) => file.as_raw_fd(),
            BackendStorage::Cow(cow) => cow.fd(),
        }
    }

    fn validate_service_req(
        &self,
        header: &BlkReqHeader,
        data_len: usize,
    ) -> Result<(u32, u64, bool), BlkBackendError> {
        let op = header.req_type & !VIRTIO_BLK_T_BARRIER;
        let force_unit_access = header.req_type & VIRTIO_BLK_T_BARRIER != 0;
        let normalized = BlkReqHeader {
            req_type: op,
            reserved: header.reserved,
            sector: header.sector,
        };
        let offset = validate_req(&normalized, data_len as u64, self.sectors)
            .map_err(|e| BlkBackendError::Validation(e.to_string()))?;
        if matches!(op, req_type::IN | req_type::OUT) {
            let device_len = self
                .sectors
                .checked_mul(SECTOR_SIZE)
                .ok_or_else(|| BlkBackendError::Validation("device length overflows".into()))?;
            let end = offset
                .checked_add(data_len as u64)
                .ok_or_else(|| BlkBackendError::Validation("request end overflows".into()))?;
            if end > device_len {
                return Err(BlkBackendError::Validation(format!(
                    "request end {end} exceeds device length {device_len}"
                )));
            }
        }
        Ok((op, offset, force_unit_access))
    }

    fn read_at(&mut self, offset: u64, data: &mut [u8]) -> Result<(), BlkBackendError> {
        match &mut self.storage {
            BackendStorage::Raw(file) => read_exact_at(file, offset, data).map_err(Into::into),
            BackendStorage::Cow(cow) => cow.read_at(offset, data),
        }
    }

    fn write_at(
        &mut self,
        offset: u64,
        data: &[u8],
        force_unit_access: bool,
    ) -> Result<(), BlkBackendError> {
        match &mut self.storage {
            BackendStorage::Raw(file) => {
                write_all_at(file, offset, data)?;
                if force_unit_access {
                    file.sync_data()?;
                }
                Ok(())
            }
            BackendStorage::Cow(cow) => cow.write_at(offset, data, force_unit_access),
        }
    }

    fn flush(&mut self) -> Result<(), BlkBackendError> {
        match &mut self.storage {
            BackendStorage::Raw(file) => file.sync_data().map_err(Into::into),
            BackendStorage::Cow(cow) => cow.flush(),
        }
    }
}

#[derive(Clone, Copy)]
struct BlockSegment {
    block: u64,
    within_block: usize,
    len: usize,
    buffer_offset: usize,
}

impl BlockSegment {
    fn buffer_range(self) -> std::ops::Range<usize> {
        self.buffer_offset..self.buffer_offset + self.len
    }

    fn absolute_offset(self) -> Option<u64> {
        self.block
            .checked_mul(SECTOR_SIZE)?
            .checked_add(self.within_block as u64)
    }
}

struct BlockSegments {
    next_offset: u64,
    remaining: usize,
    buffer_offset: usize,
}

impl BlockSegments {
    fn new(offset: u64, len: usize) -> Self {
        Self {
            next_offset: offset,
            remaining: len,
            buffer_offset: 0,
        }
    }
}

impl Iterator for BlockSegments {
    type Item = BlockSegment;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let block = self.next_offset / SECTOR_SIZE;
        let within_block = (self.next_offset % SECTOR_SIZE) as usize;
        let len = cmp::min(SECTOR_SIZE_USIZE - within_block, self.remaining);
        let segment = BlockSegment {
            block,
            within_block,
            len,
            buffer_offset: self.buffer_offset,
        };
        self.next_offset += len as u64;
        self.buffer_offset += len;
        self.remaining -= len;
        Some(segment)
    }
}

fn bitmap_len(blocks: u64) -> Result<u64, BlkBackendError> {
    blocks
        .checked_add(7)
        .map(|n| n / 8)
        .ok_or_else(|| BlkBackendError::Validation("bitmap length overflows".into()))
}

fn align_up(value: u64, align: u64) -> Result<u64, BlkBackendError> {
    value
        .checked_add(align - 1)
        .map(|v| (v / align) * align)
        .ok_or_else(|| BlkBackendError::Validation("alignment overflows".into()))
}

fn usize_from_u64(value: u64, field: &str) -> Result<usize, BlkBackendError> {
    usize::try_from(value)
        .map_err(|_| BlkBackendError::Validation(format!("{field} does not fit usize")))
}

fn validate_overlay_header(actual: CowHeader, expected: CowHeader) -> Result<(), BlkBackendError> {
    if actual.base_len != expected.base_len {
        return Err(BlkBackendError::Validation(format!(
            "overlay base length {} does not match {}",
            actual.base_len, expected.base_len
        )));
    }
    if actual.blocks != expected.blocks
        || actual.bitmap_offset != expected.bitmap_offset
        || actual.bitmap_len != expected.bitmap_len
        || actual.data_offset != expected.data_offset
    {
        return Err(BlkBackendError::Validation(
            "overlay layout does not match base image".into(),
        ));
    }
    Ok(())
}

fn validate_unused_bitmap_bits(bitmap: &[u8], blocks: u64) -> Result<(), BlkBackendError> {
    let used_bits_in_last_byte = blocks % 8;
    if used_bits_in_last_byte == 0 || bitmap.is_empty() {
        return Ok(());
    }
    let valid_mask = (1u8 << used_bits_in_last_byte) - 1;
    if bitmap[bitmap.len() - 1] & !valid_mask != 0 {
        return Err(BlkBackendError::Validation(
            "overlay bitmap has bits beyond base length".into(),
        ));
    }
    Ok(())
}

fn read_exact_at(file: &mut File, offset: u64, data: &mut [u8]) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(data)
}

fn write_all_at(file: &mut File, offset: u64, data: &[u8]) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)
}

fn read_u32(buf: &[u8], offset: usize) -> Result<u32, BlkBackendError> {
    Ok(u32::from_le_bytes(
        buf[offset..offset + 4]
            .try_into()
            .map_err(|_| BlkBackendError::Validation("u32 field truncated".into()))?,
    ))
}

fn read_u64(buf: &[u8], offset: usize) -> Result<u64, BlkBackendError> {
    Ok(u64::from_le_bytes(
        buf[offset..offset + 8]
            .try_into()
            .map_err(|_| BlkBackendError::Validation("u64 field truncated".into()))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(req_type: u32, sector: u64) -> BlkReqHeader {
        BlkReqHeader {
            req_type,
            reserved: 0,
            sector,
        }
    }

    fn write_block_pattern(path: &std::path::Path, patterns: &[u8]) {
        let mut file = File::create(path).unwrap();
        for pattern in patterns {
            file.write_all(&[*pattern; SECTOR_SIZE_USIZE]).unwrap();
        }
        file.flush().unwrap();
    }

    fn local_test_dir(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-work")
            .join(format!("{name}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn service_read(backend: &mut BlkBackend, sector: u64, len: usize) -> Vec<u8> {
        let mut data = vec![0u8; len];
        let status = backend.service(&hdr(req_type::IN, sector), &mut data);
        assert_eq!(status, status::OK);
        data
    }

    fn service_write(backend: &mut BlkBackend, sector: u64, data: &[u8]) {
        let mut data = data.to_vec();
        let status = backend.service(&hdr(req_type::OUT, sector), &mut data);
        assert_eq!(status, status::OK);
    }

    #[test]
    fn service_read_returns_data() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&[0xAA; 512]).unwrap();
        tmp.flush().unwrap();

        let mut backend = BlkBackend::open(&tmp.path().to_path_buf(), false).unwrap();

        let header = BlkReqHeader {
            req_type: req_type::IN,
            reserved: 0,
            sector: 0,
        };
        let mut data = vec![0u8; 512];
        let status = backend.service(&header, &mut data);
        assert_eq!(status, status::OK);
        assert!(data.iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn service_write_persists() {
        // Create a temp file with 512 bytes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.blk");
        std::fs::write(&path, vec![0u8; 512]).unwrap();

        let mut backend = BlkBackend::open(&path, false).unwrap();

        let header = BlkReqHeader {
            req_type: req_type::OUT,
            reserved: 0,
            sector: 0,
        };
        let mut data = vec![0xBB; 512];
        let status = backend.service(&header, &mut data);
        assert_eq!(status, status::OK);

        // Re-read to verify.
        let mut buf = vec![0u8; 512];
        let read_header = BlkReqHeader {
            req_type: req_type::IN,
            reserved: 0,
            sector: 0,
        };
        backend.service(&read_header, &mut buf);
        assert!(buf.iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn read_only_rejects_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.blk");
        std::fs::write(&path, vec![0u8; 512]).unwrap();

        let mut backend = BlkBackend::open(&path, true).unwrap();

        let header = BlkReqHeader {
            req_type: req_type::OUT,
            reserved: 0,
            sector: 0,
        };
        let mut data = vec![0xCC; 512];
        let status = backend.service(&header, &mut data);
        assert_eq!(status, status::IO_ERR);
    }

    #[test]
    fn flush_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flush.blk");
        std::fs::write(&path, vec![0u8; 512]).unwrap();

        let mut backend = BlkBackend::open(&path, false).unwrap();

        let header = BlkReqHeader {
            req_type: req_type::FLUSH,
            reserved: 0,
            sector: 0,
        };
        let status = backend.service(&header, &mut []);
        assert_eq!(status, status::OK);
    }

    #[test]
    fn cow_write_goes_to_overlay_and_leaves_base_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.img");
        let overlay = dir.path().join("vm.overlay");
        write_block_pattern(&base, &[0x11, 0x22]);
        let original_base = std::fs::read(&base).unwrap();

        let mut backend = BlkBackend::open_cow(&base, &overlay).unwrap();
        service_write(&mut backend, 0, &[0xAA; SECTOR_SIZE_USIZE]);
        assert_eq!(
            backend.service(&hdr(req_type::FLUSH, 0), &mut []),
            status::OK
        );

        assert_eq!(std::fs::read(&base).unwrap(), original_base);

        let mut overlay_file = File::open(&overlay).unwrap();
        let mut header_buf = [0u8; COW_HEADER_LEN];
        read_exact_at(&mut overlay_file, 0, &mut header_buf).unwrap();
        let header = CowHeader::decode(&header_buf).unwrap();
        let mut overlay_block = [0u8; SECTOR_SIZE_USIZE];
        read_exact_at(&mut overlay_file, header.data_offset, &mut overlay_block).unwrap();
        assert_eq!(overlay_block, [0xAA; SECTOR_SIZE_USIZE]);
    }

    #[test]
    fn cow_reads_unwritten_blocks_from_base_and_written_blocks_from_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.img");
        let overlay = dir.path().join("vm.overlay");
        write_block_pattern(&base, &[0x10, 0x20, 0x30]);

        let mut backend = BlkBackend::open_cow(&base, &overlay).unwrap();
        assert_eq!(
            service_read(&mut backend, 1, SECTOR_SIZE_USIZE),
            [0x20; 512]
        );

        service_write(&mut backend, 1, &[0xCC; SECTOR_SIZE_USIZE]);
        assert_eq!(
            service_read(&mut backend, 1, SECTOR_SIZE_USIZE),
            [0xCC; 512]
        );
        assert_eq!(
            service_read(&mut backend, 2, SECTOR_SIZE_USIZE),
            [0x30; 512]
        );
    }

    #[test]
    fn cow_overlay_dirty_bitmap_persists_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.img");
        let overlay = dir.path().join("vm.overlay");
        write_block_pattern(&base, &[0x10, 0x20]);

        {
            let mut backend = BlkBackend::open_cow(&base, &overlay).unwrap();
            service_write(&mut backend, 1, &[0xDD; SECTOR_SIZE_USIZE]);
            assert_eq!(
                backend.service(&hdr(req_type::FLUSH, 0), &mut []),
                status::OK
            );
        }

        let mut reopened = BlkBackend::open_cow(&base, &overlay).unwrap();
        assert_eq!(
            service_read(&mut reopened, 0, SECTOR_SIZE_USIZE),
            [0x10; 512]
        );
        assert_eq!(
            service_read(&mut reopened, 1, SECTOR_SIZE_USIZE),
            [0xDD; 512]
        );
    }

    #[test]
    fn cow_two_overlays_share_base_but_keep_private_writes() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.img");
        let overlay_a = dir.path().join("a.overlay");
        let overlay_b = dir.path().join("b.overlay");
        write_block_pattern(&base, &[0x44]);
        let original_base = std::fs::read(&base).unwrap();

        let mut a = BlkBackend::open_cow(&base, &overlay_a).unwrap();
        let mut b = BlkBackend::open_cow(&base, &overlay_b).unwrap();
        service_write(&mut a, 0, &[0xA1; SECTOR_SIZE_USIZE]);
        service_write(&mut b, 0, &[0xB2; SECTOR_SIZE_USIZE]);

        assert_eq!(service_read(&mut a, 0, SECTOR_SIZE_USIZE), [0xA1; 512]);
        assert_eq!(service_read(&mut b, 0, SECTOR_SIZE_USIZE), [0xB2; 512]);
        assert_eq!(std::fs::read(&base).unwrap(), original_base);
    }

    #[test]
    fn cow_restore_clones_do_not_cross_talk_or_modify_base() {
        let dir = local_test_dir("cow-restore-isolation");
        let base = dir.join("base.img");
        let overlay_a = dir.join("clone-a.overlay");
        let overlay_b = dir.join("clone-b.overlay");
        write_block_pattern(&base, &[0x44, 0x55, 0x66, 0x77]);
        let original_base = std::fs::read(&base).unwrap();

        {
            let mut a = BlkBackend::open_cow(&base, &overlay_a).unwrap();
            service_write(&mut a, 1, &[0xA1; SECTOR_SIZE_USIZE]);
            assert_eq!(a.service(&hdr(req_type::FLUSH, 0), &mut []), status::OK);
        }

        let mut b = BlkBackend::open_cow(&base, &overlay_b).unwrap();
        assert_eq!(
            service_read(&mut b, 1, SECTOR_SIZE_USIZE),
            [0x55; SECTOR_SIZE_USIZE],
            "clone B must see base data, not clone A's overlay write"
        );
        assert_eq!(std::fs::read(&base).unwrap(), original_base);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cow_open_creates_missing_sparse_overlay() {
        let dir = local_test_dir("cow-create-missing-overlay");
        let base = dir.join("base.img");
        let overlay = dir.join("fresh.overlay");
        write_block_pattern(&base, &[0x10; 16]);
        assert!(!overlay.exists());

        let backend = BlkBackend::open_cow(&base, &overlay).unwrap();

        assert!(overlay.exists());
        assert_eq!(backend.sectors, 16);
        assert!(
            std::fs::metadata(&overlay).unwrap().len() < std::fs::metadata(&base).unwrap().len(),
            "new overlay should contain only metadata until sectors are dirtied"
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cow_flush_and_fua_sync_overlay_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.img");
        let overlay = dir.path().join("vm.overlay");
        write_block_pattern(&base, &[0x55]);

        let mut backend = BlkBackend::open_cow(&base, &overlay).unwrap();
        let mut data = vec![0xEF; SECTOR_SIZE_USIZE];
        let status = backend.service(&hdr(req_type::OUT | VIRTIO_BLK_T_BARRIER, 0), &mut data);
        assert_eq!(status, status::OK);
        assert_eq!(
            backend.service(&hdr(req_type::FLUSH, 0), &mut []),
            status::OK
        );

        drop(backend);
        let mut reopened = BlkBackend::open_cow(&base, &overlay).unwrap();
        assert_eq!(
            service_read(&mut reopened, 0, SECTOR_SIZE_USIZE),
            [0xEF; 512]
        );
    }

    #[test]
    fn cow_partial_write_preserves_base_bytes_and_handles_last_block() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.img");
        let overlay = dir.path().join("vm.overlay");
        write_block_pattern(&base, &[0x10, 0x20, 0x30]);

        let mut backend = BlkBackend::open_cow(&base, &overlay).unwrap();
        service_write(&mut backend, 1, &[0x99; 600]);

        let sector_one = service_read(&mut backend, 1, SECTOR_SIZE_USIZE);
        assert_eq!(sector_one, [0x99; 512]);

        let sector_two = service_read(&mut backend, 2, SECTOR_SIZE_USIZE);
        assert_eq!(&sector_two[..88], &[0x99; 88]);
        assert_eq!(&sector_two[88..], &[0x30; SECTOR_SIZE_USIZE - 88]);
    }

    #[test]
    fn cow_rejects_request_that_crosses_device_end() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.img");
        let overlay = dir.path().join("vm.overlay");
        write_block_pattern(&base, &[0x10, 0x20]);

        let mut backend = BlkBackend::open_cow(&base, &overlay).unwrap();
        let mut data = vec![0x77; SECTOR_SIZE_USIZE + 1];
        let status = backend.service(&hdr(req_type::OUT, 1), &mut data);
        assert_eq!(status, status::IO_ERR);
    }
}
