//! Real virtio-blk backend — services virtqueue requests against a backing
//! file via pread/pwrite.

use std::cmp;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
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

fn write_fresh_cow(overlay: &mut File, header: &CowHeader) -> Result<(), BlkBackendError> {
    write_all_at(overlay, 0, &header.encode())?;
    let bitmap = vec![0u8; usize_from_u64(header.bitmap_len, "bitmap length")?];
    write_all_at(overlay, header.bitmap_offset, &bitmap)?;
    overlay.set_len(header.data_offset)?;
    Ok(())
}

/// Initialize an empty CoW overlay for `base_path` without syncing it.
///
/// Only for scratch overlays that the VMM creates in its private runtime
/// directory and removes when it exits: after a host crash the VM they belong
/// to is gone, so a durable header buys nothing and costs an fsync on every
/// restore. [`BlkBackend::open_cow`] initializes and syncs any other empty
/// overlay.
pub fn init_volatile_cow_overlay(
    overlay: &mut File,
    base_path: &Path,
) -> Result<(), BlkBackendError> {
    if overlay.metadata()?.len() != 0 {
        return Err(BlkBackendError::Validation(
            "volatile CoW overlay must start empty".into(),
        ));
    }
    let mut base_options = OpenOptions::new();
    base_options.read(true);
    #[cfg(unix)]
    base_options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let bytes = backing_size_bytes(&base_options.open(base_path)?)?;
    if bytes == 0 || bytes % SECTOR_SIZE != 0 {
        return Err(BlkBackendError::Validation(format!(
            "CoW base size {bytes} is not a positive multiple of {SECTOR_SIZE}"
        )));
    }
    write_fresh_cow(overlay, &CowHeader::new(bytes / SECTOR_SIZE)?)
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
            write_fresh_cow(&mut overlay, &expected)?;
            // A persistent overlay must reopen after a host crash even if the
            // guest never flushed, so its header is durable before first use.
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

    /// Overlay data for device offset `offset` lives at `data_offset + offset`.
    fn overlay_offset(&self, offset: u64) -> Result<u64, BlkBackendError> {
        self.header
            .data_offset
            .checked_add(offset)
            .ok_or_else(|| BlkBackendError::Validation("overlay offset overflows".into()))
    }

    /// Read in runs of sectors that come from the same file: one pread per
    /// run instead of one per 512-byte sector.
    fn read_at(&mut self, offset: u64, data: &mut [u8]) -> Result<(), BlkBackendError> {
        let mut segments = BlockSegments::new(offset, data.len()).peekable();
        while let Some(first) = segments.next() {
            let dirty = self.is_dirty(first.block);
            let start = first.buffer_offset;
            let mut end = start + first.len;
            while let Some(next) = segments.peek() {
                if self.is_dirty(next.block) != dirty {
                    break;
                }
                end = next.buffer_offset + next.len;
                segments.next();
            }
            let device_offset = offset
                .checked_add(start as u64)
                .ok_or_else(|| BlkBackendError::Validation("read offset overflows".into()))?;
            if dirty {
                let source = self.overlay_offset(device_offset)?;
                read_exact_at(&mut self.overlay, source, &mut data[start..end])?;
            } else {
                read_exact_at(&mut self.base, device_offset, &mut data[start..end])?;
            }
        }
        Ok(())
    }

    /// [`Self::read_at`] into guest buffers: one preadv per run of sectors
    /// that come from the same file.
    fn read_vectored_at(&self, offset: u64, iov: &[libc::iovec]) -> Result<(), BlkBackendError> {
        let len = iov_total(iov);
        let mut segments = BlockSegments::new(offset, len).peekable();
        while let Some(first) = segments.next() {
            let dirty = self.is_dirty(first.block);
            let start = first.buffer_offset;
            let mut end = start + first.len;
            while let Some(next) = segments.peek() {
                if self.is_dirty(next.block) != dirty {
                    break;
                }
                end = next.buffer_offset + next.len;
                segments.next();
            }
            let device_offset = offset
                .checked_add(start as u64)
                .ok_or_else(|| BlkBackendError::Validation("read offset overflows".into()))?;
            let run = iov_range(iov, start, end);
            if dirty {
                let source = self.overlay_offset(device_offset)?;
                preadv_exact(self.overlay.as_raw_fd(), &run, source)?;
            } else {
                preadv_exact(self.base.as_raw_fd(), &run, device_offset)?;
            }
        }
        Ok(())
    }

    /// Copy up the base bytes of a partially written edge sector that is not
    /// in the overlay yet, then write the whole request with one pwrite and
    /// record the newly dirty sectors with one bitmap write. Data reaches the
    /// overlay before the bitmap says to read it from there; both are durable
    /// after the next flush.
    fn write_at(
        &mut self,
        offset: u64,
        data: &[u8],
        force_unit_access: bool,
    ) -> Result<(), BlkBackendError> {
        if data.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| BlkBackendError::Validation("overlay write overflows".into()))?;
        let first_block = offset / SECTOR_SIZE;
        let last_block = (end - 1) / SECTOR_SIZE;
        for block in [first_block, last_block] {
            let block_start = block * SECTOR_SIZE;
            let covered = end.min(block_start + SECTOR_SIZE) - offset.max(block_start);
            if covered < SECTOR_SIZE && !self.is_dirty(block) {
                self.copy_base_block_to_overlay(block)?;
            }
            if first_block == last_block {
                break;
            }
        }
        let destination = self.overlay_offset(offset)?;
        write_all_at(&mut self.overlay, destination, data)?;
        self.mark_dirty_range(first_block, last_block)?;

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

    /// Mark sectors `first..=last` dirty and persist only the bitmap bytes
    /// that changed, in one write.
    fn mark_dirty_range(&mut self, first: u64, last: u64) -> Result<(), BlkBackendError> {
        let mut changed: Option<(usize, usize)> = None;
        for block in first..=last {
            let byte_idx = (block / 8) as usize;
            let bit = 1u8 << (block % 8);
            if self.bitmap[byte_idx] & bit == 0 {
                self.bitmap[byte_idx] |= bit;
                changed = Some(changed.map_or((byte_idx, byte_idx), |(lo, _)| (lo, byte_idx)));
            }
        }
        let Some((lo, hi)) = changed else {
            return Ok(());
        };
        let bitmap_offset = self
            .header
            .bitmap_offset
            .checked_add(lo as u64)
            .ok_or_else(|| BlkBackendError::Validation("bitmap offset overflows".into()))?;
        write_all_at(&mut self.overlay, bitmap_offset, &self.bitmap[lo..=hi])?;
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

#[cfg(unix)]
fn open_private_overlay(path: &Path) -> Result<File, BlkBackendError> {
    let file_name = path.file_name().ok_or_else(|| {
        BlkBackendError::Validation(format!("overlay path has no file name: {}", path.display()))
    })?;
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut parent_options = OpenOptions::new();
    parent_options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let parent = parent_options.open(parent_path).map_err(|error| {
        BlkBackendError::Validation(format!(
            "open overlay directory {}: {error}",
            parent_path.display()
        ))
    })?;
    let parent_metadata = parent.metadata()?;
    // SAFETY: geteuid has no preconditions and returns a scalar ID.
    let effective_uid = unsafe { libc::geteuid() };
    if parent_metadata.uid() != effective_uid && parent_metadata.uid() != 0 {
        return Err(BlkBackendError::Validation(format!(
            "overlay directory {} is owned by uid {}, expected {} or root",
            parent_path.display(),
            parent_metadata.uid(),
            effective_uid
        )));
    }
    if parent_metadata.mode() & 0o022 != 0 {
        return Err(BlkBackendError::Validation(format!(
            "overlay directory {} is group/world writable (mode {:04o})",
            parent_path.display(),
            parent_metadata.mode() & 0o7777
        )));
    }

    let name = std::ffi::CString::new(file_name.as_bytes()).map_err(|_| {
        BlkBackendError::Validation(format!(
            "overlay file name contains NUL: {}",
            path.display()
        ))
    })?;
    // First open an existing overlay. O_NOFOLLOW prevents a predictable leaf
    // name from redirecting the VMM onto another host file.
    // SAFETY: parent fd/name remain valid for openat; a successful descriptor
    // is transferred exactly once into File below.
    let existing_fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    let fd = if existing_fd >= 0 {
        existing_fd
    } else {
        let open_error = std::io::Error::last_os_error();
        if open_error.kind() != std::io::ErrorKind::NotFound {
            return Err(BlkBackendError::Validation(format!(
                "open overlay {} without following links: {open_error}",
                path.display()
            )));
        }
        // Missing overlays are created atomically. If another actor wins the
        // race, O_EXCL fails and we refuse to adopt the replacement.
        // SAFETY: parent fd/name remain valid, and mode is present with O_CREAT.
        let created_fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if created_fd < 0 {
            return Err(BlkBackendError::Validation(format!(
                "create private overlay {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        created_fd
    };
    // SAFETY: fd is a fresh successful openat result and is uniquely owned.
    let overlay = unsafe { File::from_raw_fd(fd) };
    let metadata = overlay.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(BlkBackendError::Validation(format!(
            "overlay is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.uid() != effective_uid {
        return Err(BlkBackendError::Validation(format!(
            "overlay {} is owned by uid {}, expected {}",
            path.display(),
            metadata.uid(),
            effective_uid
        )));
    }
    if metadata.nlink() != 1 {
        return Err(BlkBackendError::Validation(format!(
            "overlay {} has {} hard links; expected exactly one",
            path.display(),
            metadata.nlink()
        )));
    }
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 || mode & 0o600 != 0o600 {
        return Err(BlkBackendError::Validation(format!(
            "overlay {} must be owner read/write only, got mode {mode:04o}",
            path.display()
        )));
    }
    Ok(overlay)
}

#[cfg(not(unix))]
fn open_private_overlay(path: &Path) -> Result<File, BlkBackendError> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?)
}

/// A file-backed virtio-blk backend. Services read/write/flush requests
/// against a host file or a copy-on-write overlay over a read-only base.
pub struct BlkBackend {
    storage: BackendStorage,
    pub read_only: bool,
    pub sectors: u64,
    #[cfg(feature = "test-failpoints")]
    service_delay: std::time::Duration,
    #[cfg(feature = "test-failpoints")]
    delayed_services: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(feature = "test-failpoints")]
struct DelayedServiceGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);

#[cfg(feature = "test-failpoints")]
impl Drop for DelayedServiceGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(unix)]
fn validate_file_access(file: &File, read_only: bool) -> Result<(), BlkBackendError> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let access = flags & libc::O_ACCMODE;
    if access == libc::O_WRONLY || (!read_only && access != libc::O_RDWR) {
        return Err(BlkBackendError::Validation(
            "backing descriptor has incompatible access mode".into(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_file_access(_file: &File, _read_only: bool) -> Result<(), BlkBackendError> {
    Ok(())
}

#[cfg(unix)]
fn backing_size_bytes(file: &File) -> Result<u64, BlkBackendError> {
    let metadata = file.metadata()?;
    if metadata.file_type().is_file() {
        return Ok(metadata.len());
    }
    #[cfg(target_os = "linux")]
    if metadata.file_type().is_block_device() {
        const BLKGETSIZE64: libc::Ioctl = 0x8008_1272_u32 as libc::Ioctl;
        let mut bytes = 0_u64;
        if unsafe { libc::ioctl(file.as_raw_fd(), BLKGETSIZE64, &mut bytes) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        return Ok(bytes);
    }
    Err(BlkBackendError::Validation(
        "backing descriptor is not a regular file or block device".into(),
    ))
}

#[cfg(not(unix))]
fn backing_size_bytes(file: &File) -> Result<u64, BlkBackendError> {
    Ok(file.metadata()?.len())
}

impl BlkBackend {
    /// Open a backing file for the block device.
    pub fn open(path: &Path, read_only: bool) -> Result<Self, BlkBackendError> {
        let mut options = OpenOptions::new();
        options.read(true).write(!read_only);
        #[cfg(unix)]
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let file = options.open(path)?;
        Self::from_file(file, read_only, &path.display().to_string())
    }

    /// Adopt an already-open regular file or block device. This is used for
    /// orchestrator-inherited descriptors so a jailed VMM never re-resolves a
    /// provider path after admission.
    pub fn from_file(
        file: File,
        read_only: bool,
        diagnostic_label: &str,
    ) -> Result<Self, BlkBackendError> {
        validate_file_access(&file, read_only)?;
        let bytes = backing_size_bytes(&file)?;
        if bytes == 0 || bytes % SECTOR_SIZE != 0 {
            return Err(BlkBackendError::Validation(format!(
                "backing size {bytes} is not a positive multiple of {SECTOR_SIZE}"
            )));
        }
        let sectors = bytes / SECTOR_SIZE;
        log::info!(
            "blk backend: {} ({} sectors, read_only={read_only})",
            diagnostic_label,
            sectors
        );
        Ok(Self {
            storage: BackendStorage::Raw(file),
            read_only,
            sectors,
            #[cfg(feature = "test-failpoints")]
            service_delay: std::time::Duration::ZERO,
            #[cfg(feature = "test-failpoints")]
            delayed_services: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    /// Open a read-only base image with a private sparse copy-on-write overlay.
    pub fn open_cow(base_path: &Path, overlay_path: &Path) -> Result<Self, BlkBackendError> {
        let mut base_options = OpenOptions::new();
        base_options.read(true);
        #[cfg(unix)]
        base_options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let base = base_options.open(base_path)?;
        let metadata = base.metadata()?;
        #[cfg(unix)]
        if !metadata.file_type().is_file() && !metadata.file_type().is_block_device() {
            return Err(BlkBackendError::Validation(format!(
                "CoW base is not a regular file or block device: {}",
                base_path.display()
            )));
        }
        let bytes = backing_size_bytes(&base)?;
        if bytes == 0 || bytes % SECTOR_SIZE != 0 {
            return Err(BlkBackendError::Validation(format!(
                "CoW base size {bytes} is not a positive multiple of {SECTOR_SIZE}"
            )));
        }
        let sectors = bytes / SECTOR_SIZE;
        let overlay = open_private_overlay(overlay_path)?;
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
            #[cfg(feature = "test-failpoints")]
            service_delay: std::time::Duration::ZERO,
            #[cfg(feature = "test-failpoints")]
            delayed_services: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    #[cfg(feature = "test-failpoints")]
    pub fn set_test_service_delay(&mut self, delay: std::time::Duration) {
        self.service_delay = delay;
    }

    #[cfg(feature = "test-failpoints")]
    fn test_service_delay(&self) -> Option<DelayedServiceGuard> {
        if self.service_delay.is_zero() {
            return None;
        }
        self.delayed_services
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let guard = DelayedServiceGuard(std::sync::Arc::clone(&self.delayed_services));
        std::thread::sleep(self.service_delay);
        Some(guard)
    }

    #[cfg(feature = "test-failpoints")]
    pub(crate) fn test_delayed_services_counter(
        &self,
    ) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        std::sync::Arc::clone(&self.delayed_services)
    }

    /// Service a single block request.
    ///
    /// - `header`: the parsed virtio_blk_req (type, sector)
    /// - `data`: the data buffer (read: device fills this; write: guest provides this)
    ///
    /// Returns the status byte to write back to the guest's status descriptor.
    pub fn service(&mut self, header: &BlkReqHeader, data: &mut [u8]) -> u8 {
        #[cfg(feature = "test-failpoints")]
        let _delay_guard = self.test_service_delay();

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

    /// Service a read (`IN`) request straight into guest memory, with the
    /// validation and status of [`Self::service`]. `iov` lists the request's
    /// data buffers in order; it saves the bounce buffer and its copy, and
    /// reads a request in one syscall however the guest scattered it.
    ///
    /// # Safety
    ///
    /// Every iovec must describe writable memory that stays mapped, and that
    /// nothing in this process reads or writes, for the duration of the call.
    pub unsafe fn service_read_into(&mut self, header: &BlkReqHeader, iov: &[libc::iovec]) -> u8 {
        #[cfg(feature = "test-failpoints")]
        let _delay_guard = self.test_service_delay();
        if header.req_type & !VIRTIO_BLK_T_BARRIER != req_type::IN {
            log::warn!("blk: vectored service of a non-read request");
            return status::IO_ERR;
        }
        let offset = match self.validate_service_req(header, iov_total(iov)) {
            Ok((_, offset, _)) => offset,
            Err(e) => {
                log::warn!("blk: validation failed: {e}");
                return status::IO_ERR;
            }
        };
        let result = match &self.storage {
            BackendStorage::Raw(file) => {
                preadv_exact(file.as_raw_fd(), iov, offset).map_err(BlkBackendError::from)
            }
            BackendStorage::Cow(cow) => cow.read_vectored_at(offset, iov),
        };
        if let Err(e) = result {
            log::warn!("blk: read failed: {e}");
            return status::IO_ERR;
        }
        status::OK
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

/// One sector's share of a request: its sector number, and where its bytes
/// sit in the request buffer.
#[derive(Clone, Copy)]
struct BlockSegment {
    block: u64,
    len: usize,
    buffer_offset: usize,
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

fn iov_total(iov: &[libc::iovec]) -> usize {
    iov.iter().map(|v| v.iov_len).sum()
}

/// The part of `iov` covering bytes `start..end` of the whole list.
fn iov_range(iov: &[libc::iovec], start: usize, end: usize) -> Vec<libc::iovec> {
    let mut out = Vec::new();
    let mut at = 0usize;
    for v in iov {
        let (lo, hi) = (at, at + v.iov_len);
        at = hi;
        if hi <= start || lo >= end {
            continue;
        }
        let from = start.max(lo) - lo;
        let to = end.min(hi) - lo;
        out.push(libc::iovec {
            // SAFETY: `from` is within this iovec, so the pointer stays in it.
            iov_base: unsafe { v.iov_base.cast::<u8>().add(from) }.cast(),
            iov_len: to - from,
        });
    }
    out
}

/// Fill every byte of `iov` from `fd` at `offset`, retrying short reads; a
/// read past the end of the file is an error, as for `read_exact`.
fn preadv_exact(fd: std::os::fd::RawFd, iov: &[libc::iovec], offset: u64) -> std::io::Result<()> {
    let mut iov = iov.to_vec();
    iov.retain(|v| v.iov_len > 0);
    let mut first = 0usize;
    let mut offset = offset;
    while first < iov.len() {
        let count = (iov.len() - first).min(libc::UIO_MAXIOV as usize);
        let file_offset = libc::off_t::try_from(offset).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "read offset overflows")
        })?;
        // SAFETY: the iovecs describe caller-owned writable memory (see
        // `service_read_into`) and `count` stays within the list.
        let n =
            unsafe { libc::preadv(fd, iov[first..].as_ptr(), count as libc::c_int, file_offset) };
        if n < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let mut done = n as usize;
        offset += done as u64;
        while done > 0 {
            let v = &mut iov[first];
            if done >= v.iov_len {
                done -= v.iov_len;
                first += 1;
            } else {
                // SAFETY: `done` is within this iovec.
                v.iov_base = unsafe { v.iov_base.cast::<u8>().add(done) }.cast();
                v.iov_len -= done;
                done = 0;
            }
        }
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
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;

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
        #[cfg(unix)]
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        dir
    }

    fn private_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
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

        let mut backend = BlkBackend::open(tmp.path(), false).unwrap();

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
    fn from_file_enforces_access_size_and_object_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.blk");
        std::fs::write(&path, [0u8; 512]).unwrap();

        let read_only = OpenOptions::new().read(true).open(&path).unwrap();
        assert!(BlkBackend::from_file(read_only, false, "data").is_err());

        let odd_path = dir.path().join("odd.blk");
        std::fs::write(&odd_path, [0u8; 513]).unwrap();
        let odd = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&odd_path)
            .unwrap();
        assert!(BlkBackend::from_file(odd, false, "odd").is_err());

        let (socket, _peer) = UnixStream::pair().unwrap();
        let socket_fd: OwnedFd = socket.into();
        assert!(BlkBackend::from_file(File::from(socket_fd), true, "socket").is_err());
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
        let dir = private_tempdir();
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
        let dir = private_tempdir();
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
    fn reads_into_scattered_buffers_match_the_bounce_path() {
        let dir = private_tempdir();
        let base = dir.path().join("base.img");
        let overlay = dir.path().join("vm.overlay");
        let pattern: Vec<u8> = (0..16u8).map(|i| 0x10 + i).collect();
        write_block_pattern(&base, &pattern);
        let mut backend = BlkBackend::open_cow(&base, &overlay).unwrap();
        // Overlay runs inside the range, so the read alternates files.
        service_write(&mut backend, 3, &[0xA1; 2 * SECTOR_SIZE_USIZE]);
        service_write(&mut backend, 9, &[0xB2; SECTOR_SIZE_USIZE]);

        let expected = service_read(&mut backend, 1, 12 * SECTOR_SIZE_USIZE);
        // Buffers of uneven sizes that cut across sector boundaries.
        let sizes = [700usize, 1, 2047, 512, 1500, 1384];
        assert_eq!(sizes.iter().sum::<usize>(), 12 * SECTOR_SIZE_USIZE);
        let mut buffers: Vec<Vec<u8>> = sizes.iter().map(|&n| vec![0xEE; n]).collect();
        let iov: Vec<libc::iovec> = buffers
            .iter_mut()
            .map(|b| libc::iovec {
                iov_base: b.as_mut_ptr().cast(),
                iov_len: b.len(),
            })
            .collect();
        // SAFETY: the iovecs cover `buffers`, untouched until the call returns.
        let status = unsafe { backend.service_read_into(&hdr(req_type::IN, 1), &iov) };
        assert_eq!(status, status::OK);
        assert_eq!(buffers.concat(), expected);

        // Past the end of the device is refused like the bounce path.
        let mut tail = vec![0u8; 2 * SECTOR_SIZE_USIZE];
        let iov = [libc::iovec {
            iov_base: tail.as_mut_ptr().cast(),
            iov_len: tail.len(),
        }];
        // SAFETY: as above.
        let status = unsafe { backend.service_read_into(&hdr(req_type::IN, 15), &iov) };
        assert_eq!(status, status::IO_ERR);
        // And a write is never serviced through the read path.
        // SAFETY: as above.
        let status = unsafe { backend.service_read_into(&hdr(req_type::OUT, 0), &iov) };
        assert_eq!(status, status::IO_ERR);
    }

    #[test]
    fn raw_reads_into_buffers_cover_many_iovecs() {
        let dir = private_tempdir();
        let path = dir.path().join("raw.img");
        let pattern: Vec<u8> = (0..64u8).collect();
        write_block_pattern(&path, &pattern);
        let mut backend = BlkBackend::open(&path, true).unwrap();
        let expected = service_read(&mut backend, 0, 64 * SECTOR_SIZE_USIZE);
        // More iovecs than one preadv takes at a time.
        let mut buffers = vec![[0u8; 8]; 64 * SECTOR_SIZE_USIZE / 8];
        let iov: Vec<libc::iovec> = buffers
            .iter_mut()
            .map(|b| libc::iovec {
                iov_base: b.as_mut_ptr().cast(),
                iov_len: b.len(),
            })
            .collect();
        assert!(iov.len() > libc::UIO_MAXIOV as usize);
        // SAFETY: the iovecs cover `buffers`, untouched until the call returns.
        let status = unsafe { backend.service_read_into(&hdr(req_type::IN, 0), &iov) };
        assert_eq!(status, status::OK);
        assert_eq!(buffers.concat(), expected);
    }

    #[test]
    fn cow_overlay_dirty_bitmap_persists_after_reopen() {
        let dir = private_tempdir();
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
    fn cow_random_io_matches_a_byte_model_and_survives_reopen() {
        // Unaligned, cross-sector and mixed overlay/base reads and writes
        // against an in-memory copy of the device.
        let dir = private_tempdir();
        let base = dir.path().join("base.img");
        let overlay = dir.path().join("vm.overlay");
        let sectors = 96usize;
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let base_bytes: Vec<u8> = (0..sectors * SECTOR_SIZE_USIZE)
            .map(|i| (i * 7 % 251) as u8)
            .collect();
        std::fs::write(&base, &base_bytes).unwrap();
        let mut model = base_bytes.clone();
        let len = model.len() as u64;

        let mut backend = BlkBackend::open_cow(&base, &overlay).unwrap();
        for round in 0..600u64 {
            let offset = next() % len;
            let max = (len - offset).min(8 * SECTOR_SIZE);
            let size = (next() % max + 1) as usize;
            let at = offset as usize;
            if next() % 2 == 0 {
                let data: Vec<u8> = (0..size)
                    .map(|i| (round as usize + i) as u8 ^ 0xa5)
                    .collect();
                backend.write_at(offset, &data, false).unwrap();
                model[at..at + size].copy_from_slice(&data);
            } else {
                let mut got = vec![0u8; size];
                backend.read_at(offset, &mut got).unwrap();
                assert_eq!(got, model[at..at + size], "read at {offset} len {size}");
            }
        }
        let mut all = vec![0u8; model.len()];
        backend.read_at(0, &mut all).unwrap();
        assert_eq!(all, model);
        backend.flush().unwrap();
        drop(backend);

        // The bitmap and data persist; the base is untouched.
        let mut reopened = BlkBackend::open_cow(&base, &overlay).unwrap();
        let mut all = vec![0u8; model.len()];
        reopened.read_at(0, &mut all).unwrap();
        assert_eq!(all, model);
        assert_eq!(std::fs::read(&base).unwrap(), base_bytes);
    }

    #[test]
    fn cow_two_overlays_share_base_but_keep_private_writes() {
        let dir = private_tempdir();
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
    fn volatile_cow_overlay_opens_like_a_synced_one() {
        let dir = private_tempdir();
        let base = dir.path().join("base.img");
        let volatile = dir.path().join("volatile.overlay");
        let synced = dir.path().join("synced.overlay");
        write_block_pattern(&base, &[0x10, 0x20, 0x30]);

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&volatile)
            .unwrap();
        init_volatile_cow_overlay(&mut file, &base).unwrap();
        drop(BlkBackend::open_cow(&base, &synced).unwrap());
        assert_eq!(
            std::fs::read(&volatile).unwrap(),
            std::fs::read(&synced).unwrap(),
            "the volatile initializer must write exactly what open_cow writes"
        );

        let mut backend = BlkBackend::open_cow(&base, &volatile).unwrap();
        assert_eq!(
            service_read(&mut backend, 1, SECTOR_SIZE_USIZE),
            [0x20; 512]
        );
        service_write(&mut backend, 2, &[0xEE; SECTOR_SIZE_USIZE]);
        assert_eq!(
            service_read(&mut backend, 2, SECTOR_SIZE_USIZE),
            [0xEE; 512]
        );

        // Never re-initialize an overlay that already holds data.
        let mut used = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&volatile)
            .unwrap();
        assert!(init_volatile_cow_overlay(&mut used, &base).is_err());
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
        let dir = private_tempdir();
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
        let dir = private_tempdir();
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
        let dir = private_tempdir();
        let base = dir.path().join("base.img");
        let overlay = dir.path().join("vm.overlay");
        write_block_pattern(&base, &[0x10, 0x20]);

        let mut backend = BlkBackend::open_cow(&base, &overlay).unwrap();
        let mut data = vec![0x77; SECTOR_SIZE_USIZE + 1];
        let status = backend.service(&hdr(req_type::OUT, 1), &mut data);
        assert_eq!(status, status::IO_ERR);
    }

    #[cfg(unix)]
    #[test]
    fn cow_rejects_overlay_symlink_without_touching_target() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.img");
        let overlay = dir.path().join("vm.overlay");
        let victim = dir.path().join("victim");
        write_block_pattern(&base, &[0x10]);
        std::fs::write(&victim, b"untouched").unwrap();
        std::os::unix::fs::symlink(&victim, &overlay).unwrap();

        assert!(BlkBackend::open_cow(&base, &overlay).is_err());
        assert_eq!(std::fs::read(victim).unwrap(), b"untouched");
    }

    #[cfg(unix)]
    #[test]
    fn cow_rejects_world_writable_overlay_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let unsafe_dir = dir.path().join("unsafe");
        let base = dir.path().join("base.img");
        let overlay = unsafe_dir.join("vm.overlay");
        std::fs::create_dir(&unsafe_dir).unwrap();
        std::fs::set_permissions(&unsafe_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        write_block_pattern(&base, &[0x10]);

        let error = BlkBackend::open_cow(&base, &overlay)
            .err()
            .expect("unsafe overlay directory must fail");
        assert!(error.to_string().contains("group/world writable"));
        assert!(!overlay.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cow_rejects_non_private_or_hardlinked_existing_overlay() {
        use std::os::unix::fs::PermissionsExt;

        let dir = private_tempdir();
        let base = dir.path().join("base.img");
        let overlay = dir.path().join("vm.overlay");
        let alias = dir.path().join("alias.overlay");
        write_block_pattern(&base, &[0x10]);
        drop(BlkBackend::open_cow(&base, &overlay).unwrap());

        std::fs::set_permissions(&overlay, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(BlkBackend::open_cow(&base, &overlay).is_err());

        std::fs::set_permissions(&overlay, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(&overlay, &alias).unwrap();
        let error = BlkBackend::open_cow(&base, &overlay)
            .err()
            .expect("hardlinked overlay must fail");
        assert!(error.to_string().contains("hard links"));
    }

    #[cfg(unix)]
    #[test]
    fn cow_rejects_symlink_base() {
        let dir = tempfile::tempdir().unwrap();
        let real_base = dir.path().join("base.img");
        let base_link = dir.path().join("base-link.img");
        let overlay = dir.path().join("vm.overlay");
        write_block_pattern(&real_base, &[0x10]);
        std::os::unix::fs::symlink(&real_base, &base_link).unwrap();

        assert!(BlkBackend::open_cow(&base_link, &overlay).is_err());
        assert!(!overlay.exists());
    }
}
