//! UFFD lazy restore — the critical path to <10ms restore.
//!
//! PRD §9a: "Register guest memory with userfaultfd, hand the fd to a
//! userspace handler; the handler `mmap`s the snapshot file and resolves
//! each fault with a single `UFFDIO_COPY` directly from the mapping."

#![cfg(target_os = "linux")]

use crate::dirty::SoftwareDirtyBitmap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UffdRestoreError {
    #[error("userfaultfd: {0}")]
    Uffd(String),
    #[error("mmap: {0}")]
    Mmap(String),
    #[error("snapshot length mismatch: expected {expected} bytes, got {got} bytes")]
    SnapshotLength { expected: u64, got: u64 },
    #[error("snapshot range exceeds file length: end {end} > file length {file_len}")]
    SnapshotRange { end: u64, file_len: u64 },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[allow(dead_code)]
pub struct LazyRestore {
    snapshot_mmap: *const u8,
    snapshot_mmap_len: usize,
    uffd_fd: Option<OwnedFd>,
    handler_thread: Option<std::thread::JoinHandle<()>>,
    pages_served: std::sync::Arc<std::sync::atomic::AtomicU64>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

// SAFETY: `LazyRestore` owns the UFFD fd and mmap lifetime, and the raw mmap
// pointer is read-only after construction. Drop synchronizes shutdown by closing
// the fd and joining the handler thread before unmapping the snapshot.
unsafe impl Send for LazyRestore {}
// SAFETY: Shared access only observes atomics/fd ownership; mutable teardown is
// guarded by `Drop`'s unique `&mut self`, so concurrent readers cannot mutate
// the raw mapping.
unsafe impl Sync for LazyRestore {}

impl Drop for LazyRestore {
    fn drop(&mut self) {
        // Signal the handler thread to shut down.
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Close the uffd fd first — this unblocks the handler thread's
        // blocking read(uffd_fd) (returns EBADF or 0).
        if let Some(uffd_fd) = self.uffd_fd.take() {
            drop(uffd_fd);
        }
        // Join the handler thread (should exit within 1s via poll timeout).
        if let Some(handle) = self.handler_thread.take() {
            let _ = handle.join();
        }
        // Unmap the snapshot.
        if !self.snapshot_mmap.is_null() && self.snapshot_mmap_len > 0 {
            // SAFETY: `snapshot_mmap` was returned by `mmap` with exactly
            // `snapshot_mmap_len` bytes and is unmapped at most once in Drop.
            unsafe { libc::munmap(self.snapshot_mmap as *mut _, self.snapshot_mmap_len) };
        }
    }
}

/// Compute an `_IOWR` ioctl number, matching the kernel's `_IOWR(type, nr, struct_type)` macro.
///
/// The kernel encodes ioctls as:
///   direction (2 bits) | size (14 bits) | type (8 bits) | nr (8 bits)
/// where direction for _IOWR = 0xc0 (READ|WRITE).
const fn iowr(io_type: u8, nr: u8, size: usize) -> u32 {
    const IOC_WRITE: u32 = 1;
    const IOC_READ: u32 = 2;
    const IOC_NRBITS: u32 = 8;
    const IOC_TYPEBITS: u32 = 8;
    const IOC_SIZEBITS: u32 = 14;
    const IOC_NRSHIFT: u32 = 0;
    const IOC_TYPESHIFT: u32 = IOC_NRBITS;
    const IOC_SIZESHIFT: u32 = IOC_NRBITS + IOC_TYPEBITS;
    const IOC_DIRSHIFT: u32 = IOC_NRBITS + IOC_TYPEBITS + IOC_SIZEBITS;
    let dir = (IOC_READ | IOC_WRITE) << IOC_DIRSHIFT;
    let size_shifted = (size as u32 & ((1 << IOC_SIZEBITS) - 1)) << IOC_SIZESHIFT;
    let type_shifted = (io_type as u32) << IOC_TYPESHIFT;
    let nr_shifted = (nr as u32) << IOC_NRSHIFT;
    dir | size_shifted | type_shifted | nr_shifted
}

const UFFD_API: u64 = 0xAA;
const UFFDIO: u8 = 0xAA;
const UFFDIO_API: u32 = iowr(UFFDIO, 0x3F, std::mem::size_of::<UffdioApi>());
const UFFDIO_REGISTER: u32 = iowr(UFFDIO, 0x00, std::mem::size_of::<UffdioRegister>());
const UFFDIO_COPY: u32 = iowr(UFFDIO, 0x03, std::mem::size_of::<UffdioCopy>());
const UFFD_FEATURE_MISSING: u64 = 1;
const UFFDIO_REGISTER_MODE_MISSING: u64 = 1;

#[repr(C)]
struct UffdioCopy {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copy: i64,
}

#[repr(C)]
struct UffdioRegister {
    range: UffdioRange,
    mode: u64,
    /// Output field the kernel fills with the ioctls valid for this range.
    /// It is part of the ABI struct (total 32 bytes); omitting it makes the
    /// computed `_IOWR` size wrong (24 vs 32), so the kernel does not match
    /// `UFFDIO_REGISTER` and returns EINVAL.
    ioctls: u64,
}

#[repr(C)]
struct UffdioRange {
    start: u64,
    len: u64,
}

#[repr(C)]
struct UffdioApi {
    api: u64,
    features: u64,
    ioctls: u64,
}

// The kernel's uffd_msg struct is 32 bytes. We use a raw byte array to
// avoid union issues and extract the event type + fault address at known
// offsets (verified against kernel headers on Linux 6.17).
const UFFD_MSG_SIZE: usize = 32;
const UFFD_MSG_EVENT_OFFSET: usize = 0;
const UFFD_EVENT_PAGEFAULT: u8 = 0x12;
/// Absolute byte offset of `arg.pagefault.address` within the 32-byte
/// `uffd_msg`: event at 0, the arg union at 8, `pagefault.flags` at 8, and
/// `pagefault.address` at 16.
const UFFD_PAGEFAULT_ADDRESS_OFFSET: usize = 16;

pub fn start_lazy_restore(
    guest_mem_ptr: *mut u8,
    guest_mem_len: usize,
    snapshot_file: &std::fs::File,
    snapshot_offset: u64,
    snapshot_len: u64,
    host_dirty: Option<SoftwareDirtyBitmap>,
) -> Result<LazyRestore, UffdRestoreError> {
    if guest_mem_ptr.is_null() || guest_mem_len == 0 {
        return Err(UffdRestoreError::Uffd("guest memory range is empty".into()));
    }
    let guest_mem_len_u64 = u64::try_from(guest_mem_len)
        .map_err(|_| UffdRestoreError::Uffd("guest memory length overflows u64".into()))?;
    if snapshot_len != guest_mem_len_u64 {
        return Err(UffdRestoreError::SnapshotLength {
            expected: guest_mem_len_u64,
            got: snapshot_len,
        });
    }
    let snapshot_end = snapshot_offset
        .checked_add(snapshot_len)
        .ok_or_else(|| UffdRestoreError::Mmap("snapshot file range overflows".into()))?;
    let file_len = snapshot_file.metadata()?.len();
    if snapshot_end > file_len {
        return Err(UffdRestoreError::SnapshotRange {
            end: snapshot_end,
            file_len,
        });
    }

    let pages_served = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // 1. mmap the snapshot file read-only.
    // SAFETY: `sysconf(_SC_PAGESIZE)` has no memory-safety preconditions and
    // does not retain pointers; a non-positive result is handled below.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = if page_size > 0 {
        page_size as u64
    } else {
        4096
    };
    let page_size_usize = page_size as usize;
    if !(guest_mem_ptr as usize).is_multiple_of(page_size_usize)
        || !guest_mem_len.is_multiple_of(page_size_usize)
    {
        return Err(UffdRestoreError::Uffd(format!(
            "guest memory range must be page-aligned: ptr={guest_mem_ptr:p} len={guest_mem_len}"
        )));
    }

    let snapshot_mmap_offset = snapshot_offset / page_size * page_size;
    let snapshot_offset_delta = snapshot_offset - snapshot_mmap_offset;
    let snapshot_payload_len = usize::try_from(snapshot_len)
        .map_err(|_| UffdRestoreError::Mmap("snapshot length overflows usize".into()))?;
    let snapshot_offset_delta_usize = usize::try_from(snapshot_offset_delta)
        .map_err(|_| UffdRestoreError::Mmap("snapshot offset delta overflows usize".into()))?;
    let snapshot_mmap_len = snapshot_payload_len
        .checked_add(snapshot_offset_delta_usize)
        .ok_or_else(|| UffdRestoreError::Mmap("snapshot mmap length overflows usize".into()))?;
    let snapshot_mmap_offset_i64 = i64::try_from(snapshot_mmap_offset)
        .map_err(|_| UffdRestoreError::Mmap("snapshot mmap offset overflows off_t".into()))?;
    // SAFETY: the file descriptor is valid for the duration of `mmap`, the
    // offset is page-aligned and checked to fit `off_t`, and the returned
    // mapping is treated as read-only until it is unmapped in all error/Drop
    // paths.
    let snapshot_mmap = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            snapshot_mmap_len,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            snapshot_file.as_raw_fd(),
            snapshot_mmap_offset_i64,
        )
    };
    if snapshot_mmap == libc::MAP_FAILED {
        return Err(UffdRestoreError::Mmap(format!(
            "mmap snapshot: {}",
            std::io::Error::last_os_error()
        )));
    }

    // 2. Create the userfaultfd.
    // SAFETY: `syscall(SYS_userfaultfd, flags)` has no Rust memory-safety
    // preconditions. On success the returned fd is immediately wrapped in
    // `OwnedFd`; on failure no fd is owned.
    let uffd_raw =
        unsafe { libc::syscall(libc::SYS_userfaultfd, libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if uffd_raw < 0 {
        // SAFETY: `snapshot_mmap` is the successful `mmap` result above with
        // length `snapshot_mmap_len`, and has not been unmapped yet.
        unsafe { libc::munmap(snapshot_mmap, snapshot_mmap_len) };
        return Err(UffdRestoreError::Uffd(format!(
            "userfaultfd: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: `uffd_raw` is a fresh fd returned by `userfaultfd` and is not
    // aliased by any other `OwnedFd`.
    let uffd_fd = unsafe { OwnedFd::from_raw_fd(uffd_raw as RawFd) };

    // 3. UFFDIO_API.
    let mut api = UffdioApi {
        api: UFFD_API,
        features: UFFD_FEATURE_MISSING,
        ioctls: 0,
    };
    // SAFETY: `uffd_fd` is a valid userfaultfd and `api` points to an initialized
    // `uffdio_api` ABI struct for the kernel to read/write during the ioctl.
    let rc = unsafe { libc::ioctl(uffd_fd.as_raw_fd(), UFFDIO_API as _, &mut api) };
    if rc < 0 {
        // SAFETY: `snapshot_mmap` is still mapped with `snapshot_mmap_len`.
        unsafe { libc::munmap(snapshot_mmap, snapshot_mmap_len) };
        return Err(UffdRestoreError::Uffd(format!(
            "UFFDIO_API: {}",
            std::io::Error::last_os_error()
        )));
    }

    // 4. UFFDIO_REGISTER.
    let mut register = UffdioRegister {
        range: UffdioRange {
            start: guest_mem_ptr as u64,
            len: guest_mem_len as u64,
        },
        mode: UFFDIO_REGISTER_MODE_MISSING,
        ioctls: 0,
    };
    // SAFETY: `register` is an initialized `uffdio_register` ABI struct and
    // the registered guest range was checked to be non-empty and page-aligned.
    let rc = unsafe { libc::ioctl(uffd_fd.as_raw_fd(), UFFDIO_REGISTER as _, &mut register) };
    if rc < 0 {
        // SAFETY: `snapshot_mmap` is still mapped with `snapshot_mmap_len`.
        unsafe { libc::munmap(snapshot_mmap, snapshot_mmap_len) };
        return Err(UffdRestoreError::Uffd(format!(
            "UFFDIO_REGISTER: {}",
            std::io::Error::last_os_error()
        )));
    }

    // 5. Spawn the fault-handler thread.
    let uffd_fd_raw = uffd_fd.as_raw_fd();
    // SAFETY: `snapshot_offset_delta <= snapshot_mmap_len` by construction and
    // the resulting pointer stays within the live read-only mapping.
    let snapshot_ptr = unsafe { (snapshot_mmap as *const u8).add(snapshot_offset_delta_usize) };
    let snapshot_ptr_val = snapshot_ptr as usize;
    let guest_base_val = guest_mem_ptr as usize;
    let snapshot_len_clone = snapshot_payload_len;
    let pages_clone = pages_served.clone();
    let shutdown_clone = shutdown.clone();
    let handler_thread = std::thread::spawn(move || {
        fault_handler_loop(
            uffd_fd_raw,
            snapshot_ptr_val as *const u8,
            snapshot_len_clone,
            guest_base_val,
            pages_clone,
            shutdown_clone,
            host_dirty,
        );
    });

    log::info!("UFFD lazy restore started: {guest_mem_len} bytes registered");

    Ok(LazyRestore {
        snapshot_mmap: snapshot_mmap as *const u8,
        snapshot_mmap_len,
        uffd_fd: Some(uffd_fd),
        handler_thread: Some(handler_thread),
        pages_served,
        shutdown,
    })
}

/// Alias for callers that arm UFFD over an already-live guest memory mapping.
///
/// `start_lazy_restore` never allocates or replaces guest memory; it registers
/// the supplied address range in place. This wrapper makes that contract explicit
/// for suspend/resume, where the existing KVM memory slot must stay intact.
pub fn start_lazy_restore_in_place(
    guest_mem_ptr: *mut u8,
    guest_mem_len: usize,
    snapshot_file: &std::fs::File,
    snapshot_offset: u64,
    snapshot_len: u64,
    host_dirty: Option<SoftwareDirtyBitmap>,
) -> Result<LazyRestore, UffdRestoreError> {
    start_lazy_restore(
        guest_mem_ptr,
        guest_mem_len,
        snapshot_file,
        snapshot_offset,
        snapshot_len,
        host_dirty,
    )
}

/// Drop resident anonymous guest-memory pages after UFFD has been registered.
///
/// On refault, the UFFD handler rehydrates each page from the saved image instead
/// of letting the kernel zero-fill the MAP_ANONYMOUS range.
pub fn madvise_dontneed(
    guest_mem_ptr: *mut u8,
    guest_mem_len: usize,
) -> Result<(), UffdRestoreError> {
    if guest_mem_ptr.is_null() || guest_mem_len == 0 {
        return Err(UffdRestoreError::Uffd("guest memory range is empty".into()));
    }
    // SAFETY: the caller provides a valid mapped guest-memory range. `madvise`
    // does not outlive the range and cannot move it; errors are reported.
    let rc = unsafe { libc::madvise(guest_mem_ptr.cast(), guest_mem_len, libc::MADV_DONTNEED) };
    if rc < 0 {
        return Err(UffdRestoreError::Uffd(format!(
            "madvise(MADV_DONTNEED): {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn fault_handler_loop(
    uffd_fd: RawFd,
    snapshot_ptr: *const u8,
    snapshot_len: usize,
    guest_base: usize,
    pages_served: std::sync::Arc<std::sync::atomic::AtomicU64>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    host_dirty: Option<SoftwareDirtyBitmap>,
) {
    const PAGE_SIZE: usize = 4096;

    loop {
        // Check shutdown flag first.
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }

        let mut msg_buf = [0u8; UFFD_MSG_SIZE];

        // Use poll with a 500ms timeout so we can check shutdown flag.
        let mut pfd = libc::pollfd {
            fd: uffd_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` points to one initialized `pollfd`; the kernel only
        // writes within that single element during the call.
        let prc = unsafe { libc::poll(&mut pfd, 1, 500) };
        if prc <= 0 {
            if prc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            continue; // timeout — loop back, check shutdown
        }
        if pfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            break; // fd closed — exit
        }

        // SAFETY: `msg_buf` is a valid writable buffer of `UFFD_MSG_SIZE`
        // bytes and `uffd_fd` is polled readable before attempting the read.
        let n = unsafe { libc::read(uffd_fd, msg_buf.as_mut_ptr() as *mut _, UFFD_MSG_SIZE) };

        if n <= 0 {
            if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN) {
                continue;
            }
            break;
        }
        if n as usize != UFFD_MSG_SIZE {
            log::warn!("partial userfaultfd read: {n} of {UFFD_MSG_SIZE} bytes");
            continue;
        }

        // Only pagefault events carry a fault address; skip anything else
        // (fork/remap/unmap events) rather than misparsing it as an address.
        if msg_buf[UFFD_MSG_EVENT_OFFSET] != UFFD_EVENT_PAGEFAULT {
            continue;
        }
        let mut fault_addr_bytes = [0u8; 8];
        fault_addr_bytes.copy_from_slice(
            &msg_buf[UFFD_PAGEFAULT_ADDRESS_OFFSET..UFFD_PAGEFAULT_ADDRESS_OFFSET + 8],
        );
        let fault_addr = u64::from_le_bytes(fault_addr_bytes);

        // UFFDIO_COPY requires a page-aligned destination. Align the faulting
        // address down to its page, source the matching page from the snapshot
        // mapping, and resolve the whole page in one copy.
        let Ok(fault_addr_usize) = usize::try_from(fault_addr) else {
            log::warn!("UFFD fault address 0x{fault_addr:x} overflows usize");
            continue;
        };
        let Some(guest_end) = guest_base.checked_add(snapshot_len) else {
            log::warn!("UFFD guest range overflows: base=0x{guest_base:x} len={snapshot_len}");
            break;
        };
        if fault_addr_usize < guest_base || fault_addr_usize >= guest_end {
            log::warn!("UFFD fault address 0x{fault_addr:x} outside registered guest range");
            continue;
        }
        let guest_offset = fault_addr_usize - guest_base;
        let page_offset = guest_offset & !(PAGE_SIZE - 1);
        let Some(page_end) = page_offset.checked_add(PAGE_SIZE) else {
            log::warn!("UFFD page offset overflows: 0x{page_offset:x}");
            continue;
        };
        if page_end > snapshot_len {
            log::warn!("UFFD page offset 0x{page_offset:x} beyond snapshot length {snapshot_len}");
            continue;
        }
        let Some(dst_usize) = guest_base.checked_add(page_offset) else {
            log::warn!(
                "UFFD destination address overflows: base=0x{guest_base:x} page=0x{page_offset:x}"
            );
            continue;
        };
        let Ok(dst) = u64::try_from(dst_usize) else {
            log::warn!("UFFD destination address 0x{dst_usize:x} overflows u64");
            continue;
        };
        // SAFETY: `page_offset..page_offset + PAGE_SIZE` was checked to be
        // within the snapshot mapping, and `snapshot_ptr` points at its payload.
        let src = unsafe { snapshot_ptr.add(page_offset) };

        let copy = UffdioCopy {
            dst,
            src: src as u64,
            len: PAGE_SIZE as u64,
            mode: 0,
            copy: 0,
        };

        // SAFETY: `copy` references a page-aligned destination inside the
        // registered guest range and a page inside the read-only snapshot mmap.
        let rc = unsafe { libc::ioctl(uffd_fd, UFFDIO_COPY as _, &copy) };
        if rc < 0 {
            log::warn!("UFFDIO_COPY failed at 0x{fault_addr:x}");
        } else {
            if let Some(dirty) = host_dirty.as_ref() {
                dirty.mark_range(page_offset as u64, PAGE_SIZE as u64);
            }
            pages_served.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    log::info!(
        "UFFD fault handler: served {} pages",
        pages_served.load(std::sync::atomic::Ordering::Relaxed)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uffdio_api_constant_matches_kernel() {
        // UFFDIO_API = _IOWR(0xAA, 0x3F, struct uffdio_api)
        // struct uffdio_api is { u64 api; u64 features; u64 ioctls; } = 24 bytes.
        // Expected: 0xC018AA3F
        assert_eq!(
            UFFDIO_API, 0xC018AA3F,
            "UFFDIO_API must match kernel _IOWR(0xAA, 0x3F, uffdio_api)"
        );
    }

    #[test]
    fn uffdio_register_constant_matches_kernel() {
        // UFFDIO_REGISTER = _IOWR(0xAA, 0x00, struct uffdio_register)
        // struct uffdio_register is
        //   { struct uffdio_range range; u64 mode; u64 ioctls; } = 32 bytes
        // (the trailing `ioctls` is an output field, part of the ABI struct).
        // Expected: 0xC020AA00. Verified against kernel headers on c8i
        // (sizeof(struct uffdio_register)=32).
        assert_eq!(
            UFFDIO_REGISTER, 0xC020AA00,
            "UFFDIO_REGISTER must match kernel _IOWR(0xAA, 0x00, uffdio_register)"
        );
    }

    #[test]
    fn uffdio_copy_constant_matches_kernel() {
        // UFFDIO_COPY = _IOWR(0xAA, 0x03, struct uffdio_copy)
        // struct uffdio_copy is { u64 dst; u64 src; u64 len; u64 mode; i64 copy; } = 40 bytes.
        // Expected: 0xC028AA03
        assert_eq!(
            UFFDIO_COPY, 0xC028AA03,
            "UFFDIO_COPY must match kernel _IOWR(0xAA, 0x03, uffdio_copy)"
        );
    }

    #[test]
    fn uffdio_copy_size() {
        let size = std::mem::size_of::<UffdioCopy>();
        assert!(size == 40, "UffdioCopy size = {size}, expected 40");
    }

    #[test]
    fn uffdio_register_size() {
        let size = std::mem::size_of::<UffdioRegister>();
        assert!(size == 32, "UffdioRegister size = {size}, expected 32");
    }

    #[test]
    fn uffdio_api_size() {
        let size = std::mem::size_of::<UffdioApi>();
        assert!(size == 24, "UffdioApi size = {size}, expected 24");
    }

    #[test]
    fn iowr_macro_correctness() {
        // Verify our iowr() against known kernel values.
        // _IOWR(type, nr, struct) = (1|2)<<30 | sizeof(struct)<<16 | type<<8 | nr
        assert_eq!(iowr(0xAA, 0x3F, 24), 0xC018AA3F);
        assert_eq!(iowr(0xAA, 0x00, 32), 0xC020AA00);
        assert_eq!(iowr(0xAA, 0x03, 40), 0xC028AA03);
    }

    #[test]
    fn fault_handler_offset_math() {
        // Simulate the offset computation from fault_handler_loop.
        // If guest_base = 0x7f0000000000 and fault_addr = 0x7f0000001000,
        // the guest_offset should be 0x1000, and page_offset should be 0x1000.
        let guest_base: usize = 0x7f0000000000;
        let snapshot_len: usize = 64 * 1024 * 1024; // 64 MB snapshot
        const PAGE_SIZE: usize = 4096;

        // Fault at guest_base + 0x1000 (page 1 of guest memory)
        let fault_addr: usize = guest_base + 0x1000;
        let guest_end = guest_base + snapshot_len;
        assert!(fault_addr >= guest_base && fault_addr < guest_end);
        let guest_offset = fault_addr - guest_base;
        let page_offset = guest_offset & !(PAGE_SIZE - 1);
        assert!(page_offset + PAGE_SIZE <= snapshot_len);
        let src_offset = page_offset;
        assert_eq!(guest_offset, 0x1000);
        assert_eq!(page_offset, 0x1000);
        assert_eq!(src_offset, 0x1000);

        // Fault at guest_base + 0x5000 (page 5)
        let fault_addr2: usize = guest_base + 0x5000;
        assert!(fault_addr2 >= guest_base && fault_addr2 < guest_end);
        let guest_offset2 = fault_addr2 - guest_base;
        let page_offset2 = guest_offset2 & !(PAGE_SIZE - 1);
        assert!(page_offset2 + PAGE_SIZE <= snapshot_len);
        let src_offset2 = page_offset2;
        assert_eq!(src_offset2, 0x5000);

        // Fault beyond snapshot — reject instead of clamping to the last page.
        let fault_addr3: usize = guest_base + snapshot_len + 0x10000;
        assert!(fault_addr3 >= guest_end);
    }
}
