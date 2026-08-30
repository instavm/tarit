//! UFFD lazy restore — the critical path to <10ms restore.
//!
//! Register guest memory with userfaultfd, hand the fd to a
//! userspace handler; the handler `mmap`s the snapshot file and resolves
//! each fault with a single `UFFDIO_COPY` directly from the mapping.

#![cfg(target_os = "linux")]

use crate::dirty::SoftwareDirtyBitmap;
use sha2::{Digest, Sha256};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UffdRestoreError {
    #[error("userfaultfd syscall: {0}")]
    Userfaultfd(#[source] std::io::Error),
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
    page_discard: LazyPageDiscard,
}

/// Coordinates an intentional guest-page discard with the UFFD fault handler.
/// Without this fence a later missing-page fault would resurrect snapshot data
/// that virtio-balloon had explicitly returned to the host.
#[derive(Clone)]
pub struct LazyPageDiscard {
    guest_base: usize,
    guest_len: usize,
    discarded_pages: std::sync::Arc<std::sync::Mutex<Vec<bool>>>,
    // Excludes MADV_DONTNEED while a host-side snapshot reader is walking the
    // UFFD-backed range. The fault handler intentionally does not take this
    // fence: it must always remain able to resolve the snapshot reader's
    // missing-page fault.
    snapshot_fence: std::sync::Arc<std::sync::RwLock<()>>,
}

impl LazyPageDiscard {
    pub fn snapshot_fence(&self) -> std::sync::Arc<std::sync::RwLock<()>> {
        self.snapshot_fence.clone()
    }

    pub fn discard(&self, offset: usize, len: usize) -> Result<(), UffdRestoreError> {
        const PAGE_SIZE: usize = 4096;
        if len == 0
            || !offset.is_multiple_of(PAGE_SIZE)
            || !len.is_multiple_of(PAGE_SIZE)
            || offset
                .checked_add(len)
                .is_none_or(|end| end > self.guest_len)
        {
            return Err(UffdRestoreError::Uffd(
                "discard range must be non-empty, page-aligned, and inside guest memory".into(),
            ));
        }
        let _snapshot_exclusion = self
            .snapshot_fence
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut discarded = self
            .discarded_pages
            .lock()
            .map_err(|_| UffdRestoreError::Uffd("discard bitmap lock poisoned".into()))?;
        let page_range = offset / PAGE_SIZE..(offset + len) / PAGE_SIZE;
        for page in page_range.clone() {
            discarded[page] = true;
        }
        // Do not hold the bitmap mutex through MADV_DONTNEED. More
        // importantly, never madvise a missing page: the kernel call may wait
        // for its UFFD fault while the handler is concurrently resolving that
        // same range. Missing pages already consume no resident RAM and the
        // bitmap makes their eventual fault resolve to zero. `mincore` lets us
        // reclaim only pages that are presently resident.
        //
        // * copy first -> discard marks and removes the copied page;
        // * discard first -> the handler observes the bit and copies zeroes.
        //
        // A failed madvise deliberately leaves the bit set. Rolling it back
        // could resurrect snapshot bytes on a later fault after the guest has
        // already relinquished the page.
        drop(discarded);
        let address = self
            .guest_base
            .checked_add(offset)
            .ok_or_else(|| UffdRestoreError::Uffd("discard address overflows".into()))?;
        for page_offset in (0..len).step_by(PAGE_SIZE) {
            let page_address = address
                .checked_add(page_offset)
                .ok_or_else(|| UffdRestoreError::Uffd("discard page overflows".into()))?;
            let mut residency = 0u8;
            // SAFETY: this page-aligned page is inside the validated live mmap;
            // mincore writes exactly one residency byte for one page.
            let rc = unsafe {
                libc::mincore(page_address as *mut libc::c_void, PAGE_SIZE, &mut residency)
            };
            if rc < 0 {
                return Err(UffdRestoreError::Uffd(format!(
                    "mincore before balloon discard: {}",
                    std::io::Error::last_os_error()
                )));
            }
            if residency & 1 == 0 {
                continue;
            }
            // SAFETY: the resident page is inside the guest mmap. Missing
            // pages are skipped above, preventing a UFFD/madvise wait cycle.
            let rc = unsafe {
                libc::madvise(
                    page_address as *mut libc::c_void,
                    PAGE_SIZE,
                    libc::MADV_DONTNEED,
                )
            };
            if rc < 0 {
                return Err(UffdRestoreError::Uffd(format!(
                    "madvise balloon discard: {}",
                    std::io::Error::last_os_error()
                )));
            }
        }
        Ok(())
    }
}

impl LazyRestore {
    pub fn page_discard(&self) -> LazyPageDiscard {
        self.page_discard.clone()
    }
}

#[derive(Debug, Clone)]
pub struct ChunkIntegrity {
    pub chunk_size: usize,
    pub chunk_hashes: Vec<[u8; 32]>,
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
// Ask the kernel to report MADV_DONTNEED/MADV_REMOVE operations. A pending
// REMOVE event makes UFFDIO_COPY return EAGAIN, so a production handler must
// drain these events and retry deferred faults rather than losing the fault and
// hanging the caller forever.
const UFFD_FEATURE_EVENT_REMOVE: u64 = 1 << 3;
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
const UFFD_EVENT_REMOVE: u8 = 0x15;
/// Absolute byte offset of `arg.pagefault.address` within the 32-byte
/// `uffd_msg`: event at 0, the arg union at 8, `pagefault.flags` at 8, and
/// `pagefault.address` at 16.
const UFFD_PAGEFAULT_ADDRESS_OFFSET: usize = 16;
const UFFD_REMOVE_START_OFFSET: usize = 8;
const UFFD_REMOVE_END_OFFSET: usize = 16;

pub fn start_lazy_restore(
    guest_mem_ptr: *mut u8,
    guest_mem_len: usize,
    snapshot_file: &std::fs::File,
    snapshot_offset: u64,
    snapshot_len: u64,
    host_dirty: Option<SoftwareDirtyBitmap>,
) -> Result<LazyRestore, UffdRestoreError> {
    start_lazy_restore_with_integrity(
        guest_mem_ptr,
        guest_mem_len,
        snapshot_file,
        snapshot_offset,
        snapshot_len,
        host_dirty,
        None,
    )
}

pub fn start_lazy_restore_with_integrity(
    guest_mem_ptr: *mut u8,
    guest_mem_len: usize,
    snapshot_file: &std::fs::File,
    snapshot_offset: u64,
    snapshot_len: u64,
    host_dirty: Option<SoftwareDirtyBitmap>,
    chunk_integrity: Option<ChunkIntegrity>,
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
    let discarded_pages = std::sync::Arc::new(std::sync::Mutex::new(vec![
        false;
        guest_mem_len
            .div_ceil(4096)
    ]));
    let snapshot_fence = std::sync::Arc::new(std::sync::RwLock::new(()));

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
    if let Some(integrity) = chunk_integrity.as_ref() {
        if integrity.chunk_size < page_size_usize
            || !integrity.chunk_size.is_power_of_two()
            || !integrity.chunk_size.is_multiple_of(page_size_usize)
        {
            return Err(UffdRestoreError::Uffd(
                "integrity chunk size must be a page-aligned power of two".into(),
            ));
        }
        let expected_chunks = guest_mem_len.div_ceil(integrity.chunk_size);
        if integrity.chunk_hashes.len() != expected_chunks {
            return Err(UffdRestoreError::Uffd(format!(
                "integrity manifest has {} chunks, expected {expected_chunks}",
                integrity.chunk_hashes.len()
            )));
        }
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
        return Err(UffdRestoreError::Userfaultfd(
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: `uffd_raw` is a fresh fd returned by `userfaultfd` and is not
    // aliased by any other `OwnedFd`.
    let uffd_fd = unsafe { OwnedFd::from_raw_fd(uffd_raw as RawFd) };

    // 3. UFFDIO_API.
    let mut api = UffdioApi {
        api: UFFD_API,
        features: UFFD_FEATURE_EVENT_REMOVE,
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
    let handler_shutdown = shutdown.clone();
    let handler_discarded_pages = discarded_pages.clone();
    let handler_thread = std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fault_handler_loop(FaultHandlerContext {
                uffd_fd_raw,
                snapshot_ptr: snapshot_ptr_val,
                snapshot_len: snapshot_len_clone,
                guest_base: guest_base_val,
                pages_served: pages_clone,
                shutdown: shutdown_clone,
                host_dirty,
                chunk_integrity,
                discarded_pages: handler_discarded_pages,
            });
        }));
        if !handler_shutdown.load(std::sync::atomic::Ordering::Acquire) {
            let reason = if outcome.is_err() {
                "panicked"
            } else {
                "exited"
            };
            eprintln!("SECURITY: userfaultfd handler {reason} unexpectedly; terminating VMM");
            log::error!("userfaultfd handler {reason} unexpectedly");
            // An absent UFFD handler leaves guest accesses blocked forever.
            // This process owns one VM, so fail closed and let the orchestrator
            // reconcile the exit instead of retaining an unresponsive VM.
            // SAFETY: `_exit` terminates the current VMM without running
            // destructors that may themselves touch the unresolved mapping.
            unsafe { libc::_exit(78) }
        }
    });

    log::info!("UFFD lazy restore started: {guest_mem_len} bytes registered");

    Ok(LazyRestore {
        snapshot_mmap: snapshot_mmap as *const u8,
        snapshot_mmap_len,
        uffd_fd: Some(uffd_fd),
        handler_thread: Some(handler_thread),
        pages_served,
        shutdown,
        page_discard: LazyPageDiscard {
            guest_base: guest_mem_ptr as usize,
            guest_len: guest_mem_len,
            discarded_pages,
            snapshot_fence,
        },
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

struct FaultHandlerContext {
    uffd_fd_raw: RawFd,
    snapshot_ptr: usize,
    snapshot_len: usize,
    guest_base: usize,
    pages_served: std::sync::Arc<std::sync::atomic::AtomicU64>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    host_dirty: Option<SoftwareDirtyBitmap>,
    chunk_integrity: Option<ChunkIntegrity>,
    discarded_pages: std::sync::Arc<std::sync::Mutex<Vec<bool>>>,
}

fn fault_handler_loop(context: FaultHandlerContext) {
    const PAGE_SIZE: usize = 4096;
    let FaultHandlerContext {
        uffd_fd_raw: uffd_fd,
        snapshot_ptr,
        snapshot_len,
        guest_base,
        pages_served,
        shutdown,
        host_dirty,
        chunk_integrity,
        discarded_pages,
    } = context;
    let snapshot_ptr = snapshot_ptr as *const u8;
    let mut pending_faults = std::collections::VecDeque::<u64>::new();

    loop {
        // Check shutdown flag first.
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }

        // Drain every currently queued event before issuing UFFDIO_COPY. Linux
        // returns EAGAIN from all UFFD ioctls while a REMOVE event is pending;
        // servicing a pagefault before draining the causally related REMOVE is
        // therefore both incorrect and capable of hanging the faulting thread.
        let mut pfd = libc::pollfd {
            fd: uffd_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` points to one initialized `pollfd`; the kernel only
        // writes within that single element during the call.
        let timeout_ms = if pending_faults.is_empty() { 500 } else { 0 };
        let prc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if prc < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            log::error!("userfaultfd poll failed: {error}");
            break;
        }
        if prc == 0 && pending_faults.is_empty() {
            continue; // timeout — loop back, check shutdown
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            break; // fd closed — exit
        }
        if prc > 0 && pfd.revents & libc::POLLIN != 0 {
            loop {
                let mut msg_buf = [0u8; UFFD_MSG_SIZE];
                // SAFETY: the UFFD is nonblocking and `msg_buf` is exactly one
                // kernel `uffd_msg`; repeated reads drain the ready queue.
                let n = unsafe { libc::read(uffd_fd, msg_buf.as_mut_ptr().cast(), UFFD_MSG_SIZE) };
                if n < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::EAGAIN) {
                        break;
                    }
                    if error.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    log::error!("userfaultfd read failed: {error}");
                    return;
                }
                if n == 0 {
                    return;
                }
                if n as usize != UFFD_MSG_SIZE {
                    log::error!("partial userfaultfd read: {n} of {UFFD_MSG_SIZE} bytes");
                    return;
                }

                match msg_buf[UFFD_MSG_EVENT_OFFSET] {
                    UFFD_EVENT_PAGEFAULT => {
                        let mut bytes = [0u8; 8];
                        bytes.copy_from_slice(
                            &msg_buf
                                [UFFD_PAGEFAULT_ADDRESS_OFFSET..UFFD_PAGEFAULT_ADDRESS_OFFSET + 8],
                        );
                        pending_faults.push_back(u64::from_ne_bytes(bytes));
                    }
                    UFFD_EVENT_REMOVE => {
                        // Drain REMOVE before retrying UFFDIO_COPY, but do not
                        // infer zero-on-refault from the kernel event alone.
                        // Tarit uses MADV_DONTNEED both for balloon discard
                        // (zero semantics, explicitly pre-marked by
                        // LazyPageDiscard) and for suspend eviction (rehydrate
                        // from the new image). Conflating them corrupts every
                        // suspended guest by restoring all-zero RAM.
                        let mut start_bytes = [0u8; 8];
                        let mut end_bytes = [0u8; 8];
                        start_bytes.copy_from_slice(
                            &msg_buf[UFFD_REMOVE_START_OFFSET..UFFD_REMOVE_START_OFFSET + 8],
                        );
                        end_bytes.copy_from_slice(
                            &msg_buf[UFFD_REMOVE_END_OFFSET..UFFD_REMOVE_END_OFFSET + 8],
                        );
                        let start = u64::from_ne_bytes(start_bytes) as usize;
                        let end = u64::from_ne_bytes(end_bytes) as usize;
                        let Some(guest_end) = guest_base.checked_add(snapshot_len) else {
                            log::error!("UFFD guest range overflow while handling REMOVE");
                            return;
                        };
                        if start >= end || start < guest_base || end > guest_end {
                            log::warn!(
                                "UFFD REMOVE range 0x{start:x}..0x{end:x} outside guest memory"
                            );
                        }
                    }
                    event => log::warn!("unexpected userfaultfd event 0x{event:02x}"),
                }
            }
        }

        #[cfg(feature = "test-failpoints")]
        if !pending_faults.is_empty()
            && std::env::var_os("TARIT_TEST_UFFD_HANDLER_FAILURE").as_deref()
                == Some(std::ffi::OsStr::new("after_event"))
        {
            log::error!("test failpoint: forcing userfaultfd handler exit");
            return;
        }

        let Some(fault_addr) = pending_faults.pop_front() else {
            continue;
        };

        // UFFDIO_COPY requires a page-aligned destination. Align the faulting
        // address down to its page, source the matching page from the snapshot
        // mapping, and resolve the whole page in one copy.
        let Ok(fault_addr_usize) = usize::try_from(fault_addr) else {
            log::error!("UFFD fault address 0x{fault_addr:x} overflows usize");
            break;
        };
        let Some(guest_end) = guest_base.checked_add(snapshot_len) else {
            log::warn!("UFFD guest range overflows: base=0x{guest_base:x} len={snapshot_len}");
            break;
        };
        if fault_addr_usize < guest_base || fault_addr_usize >= guest_end {
            log::error!("UFFD fault address 0x{fault_addr:x} outside registered guest range");
            break;
        }
        let guest_offset = fault_addr_usize - guest_base;
        let page_offset = guest_offset & !(PAGE_SIZE - 1);
        let copy_offset = chunk_integrity.as_ref().map_or(page_offset, |integrity| {
            page_offset & !(integrity.chunk_size - 1)
        });
        let copy_len = chunk_integrity.as_ref().map_or(PAGE_SIZE, |integrity| {
            integrity.chunk_size.min(snapshot_len - copy_offset)
        });
        let Some(copy_end) = copy_offset.checked_add(copy_len) else {
            log::error!("UFFD copy offset overflows: 0x{copy_offset:x}");
            break;
        };
        if copy_end > snapshot_len || !copy_len.is_multiple_of(PAGE_SIZE) {
            log::error!("UFFD copy range 0x{copy_offset:x}..0x{copy_end:x} is invalid");
            break;
        }
        let Some(dst_usize) = guest_base.checked_add(copy_offset) else {
            log::error!(
                "UFFD destination address overflows: base=0x{guest_base:x} offset=0x{copy_offset:x}"
            );
            break;
        };
        let Ok(dst) = u64::try_from(dst_usize) else {
            log::error!("UFFD destination address 0x{dst_usize:x} overflows u64");
            break;
        };
        // A discard racing a fault must either happen after this copy (and
        // madvise it away), or be reflected as zeroes in the bytes below.
        let discarded = match discarded_pages.lock() {
            Ok(discarded) => discarded,
            Err(_) => {
                log::error!("UFFD discard bitmap lock poisoned");
                break;
            }
        };

        // Authenticated restores copy the complete chunk into private memory,
        // hash those stable bytes, and only then hand that same buffer to the
        // kernel. This closes the mmap hash/copy TOCTOU and amortizes one
        // verification across every page prefetched by UFFDIO_COPY.
        let mut owned_chunk = chunk_integrity.as_ref().map(|integrity| {
            // SAFETY: the checked copy range is inside the read-only mapping.
            let source =
                unsafe { std::slice::from_raw_parts(snapshot_ptr.add(copy_offset), copy_len) };
            let bytes = source.to_vec();
            let chunk_index = copy_offset / integrity.chunk_size;
            if !chunk_sha256_matches(&bytes, &integrity.chunk_hashes[chunk_index]) {
                eprintln!(
                    "SECURITY: snapshot integrity failure at chunk {chunk_index} (offset 0x{copy_offset:x}); terminating VMM"
                );
                log::error!(
                    "snapshot integrity failure at chunk {chunk_index} (offset 0x{copy_offset:x})"
                );
                // There is no safe byte sequence with which to resolve this
                // fault. Terminate the compromised VMM rather than hang a vCPU
                // or let unverified state execute.
                // SAFETY: `_exit` terminates the current VMM process without
                // running destructors; the orchestrator reconciles the exit.
                unsafe { libc::_exit(78) }
            }
            bytes
        });
        let first_page = copy_offset / PAGE_SIZE;
        let page_count = copy_len / PAGE_SIZE;
        let has_discarded_page = (first_page..first_page + page_count)
            .any(|page| discarded.get(page).copied().unwrap_or(false));
        if has_discarded_page {
            let bytes = owned_chunk.get_or_insert_with(|| {
                // SAFETY: the checked copy range is inside the read-only
                // snapshot mapping and is copied before the mapping can drop.
                unsafe { std::slice::from_raw_parts(snapshot_ptr.add(copy_offset), copy_len) }
                    .to_vec()
            });
            for relative_page in 0..page_count {
                if discarded
                    .get(first_page + relative_page)
                    .copied()
                    .unwrap_or(false)
                {
                    let start = relative_page * PAGE_SIZE;
                    bytes[start..start + PAGE_SIZE].fill(0);
                }
            }
        }
        // SAFETY: the checked copy range is inside the read-only mapping. For
        // authenticated restores the owned Vec remains live through ioctl.
        let src = owned_chunk.as_ref().map_or_else(
            || unsafe { snapshot_ptr.add(copy_offset) },
            |bytes| bytes.as_ptr(),
        );

        let copy = UffdioCopy {
            dst,
            src: src as u64,
            len: copy_len as u64,
            mode: 0,
            copy: 0,
        };

        // SAFETY: `copy` references a page-aligned destination inside the
        // registered guest range and a page inside the read-only snapshot mmap.
        let mut rc = unsafe { libc::ioctl(uffd_fd, UFFDIO_COPY as _, &copy) };
        let mut resolved_offset = copy_offset;
        let mut resolved_len = copy_len;
        if rc < 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST)
            && copy_len > PAGE_SIZE
        {
            // Chunk prefetch is an optimization, not an atomic replacement of
            // mixed-residency memory. If any page in the chunk already exists,
            // Linux rejects the whole UFFDIO_COPY with EEXIST and leaves the
            // actual missing page blocked. The full chunk has already been
            // authenticated above, so safely fall back to copying only the
            // faulting page from those same verified bytes.
            let fallback_src = unsafe { src.add(page_offset - copy_offset) };
            let fallback = UffdioCopy {
                dst: (guest_base + page_offset) as u64,
                src: fallback_src as u64,
                len: PAGE_SIZE as u64,
                mode: 0,
                copy: 0,
            };
            // SAFETY: fallback covers exactly the page containing the checked
            // fault address, and its source is within the verified chunk.
            rc = unsafe { libc::ioctl(uffd_fd, UFFDIO_COPY as _, &fallback) };
            resolved_offset = page_offset;
            resolved_len = PAGE_SIZE;
        }
        if rc < 0 {
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                // A REMOVE event raced the queue drain. Preserve the fault and
                // retry only after the next iteration drains that event.
                Some(libc::EAGAIN) => pending_faults.push_back(fault_addr),
                // Another fault for the prefetched chunk may have populated
                // this page already; the blocked access is no longer pending.
                Some(libc::EEXIST) => {}
                // Dropping an unresolved fault would hang the vCPU/control
                // thread forever. Fail closed so the orchestrator can reap and
                // retry the VM instead of leaving an immortal half-VM.
                _ => {
                    eprintln!(
                        "SECURITY: unrecoverable UFFDIO_COPY failure at 0x{fault_addr:x}: {error}"
                    );
                    log::error!("UFFDIO_COPY failed at 0x{fault_addr:x}: {error}");
                    // SAFETY: unresolved UFFD faults cannot be recovered by
                    // this handler; immediate process exit avoids deadlock.
                    unsafe { libc::_exit(78) }
                }
            }
        } else {
            if let Some(dirty) = host_dirty.as_ref() {
                dirty.mark_range(resolved_offset as u64, resolved_len as u64);
            }
            pages_served.fetch_add(
                (resolved_len / PAGE_SIZE) as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    log::info!(
        "UFFD fault handler: served {} pages",
        pages_served.load(std::sync::atomic::Ordering::Relaxed)
    );
}

fn chunk_sha256_matches(bytes: &[u8], expected: &[u8; 32]) -> bool {
    let actual: [u8; 32] = Sha256::digest(bytes).into();
    actual == *expected
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::io::Write;

    #[cfg(feature = "test-failpoints")]
    #[test]
    fn forced_handler_failure_child() {
        if std::env::var_os("TARIT_TEST_UFFD_CHILD").as_deref() != Some(std::ffi::OsStr::new("1")) {
            return;
        }

        const LEN: usize = 4096;
        let file = tempfile::tempfile().expect("create snapshot file");
        file.set_len(LEN as u64).expect("size snapshot file");
        // SAFETY: creates one private page-aligned mapping which the process
        // owns until the fail-closed child exits.
        let guest = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(guest, libc::MAP_FAILED);
        let _lazy = match start_lazy_restore(guest.cast(), LEN, &file, 0, LEN as u64, None) {
            Ok(lazy) => lazy,
            Err(UffdRestoreError::Userfaultfd(error))
                if error.raw_os_error() == Some(libc::EPERM) =>
            {
                // GitHub-hosted Linux runners commonly deny unprivileged UFFD.
                // SAFETY: this dedicated subprocess has no cleanup to preserve.
                unsafe { libc::_exit(77) }
            }
            Err(error) => panic!("start lazy restore: {error}"),
        };
        // The handler consumes this event and then takes the test-only failure
        // path. The parent requires the process-wide fail-closed exit code.
        // SAFETY: `guest` is the live UFFD-registered mapping above.
        let _ = unsafe { std::ptr::read_volatile(guest.cast::<u8>()) };
        panic!("fault unexpectedly resolved after forced handler failure");
    }

    #[cfg(feature = "test-failpoints")]
    #[test]
    fn unexpected_handler_exit_terminates_the_vmm_process() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "uffd_restore::tests::forced_handler_failure_child",
                "--nocapture",
            ])
            .env("TARIT_TEST_UFFD_CHILD", "1")
            .env("TARIT_TEST_UFFD_HANDLER_FAILURE", "after_event")
            .status()
            .expect("run UFFD failure subprocess");
        if status.code() == Some(77) {
            return;
        }
        assert_eq!(status.code(), Some(78));
    }

    #[test]
    fn authenticated_chunk_rejects_single_byte_corruption() {
        let original = vec![0x5a; 64 * 1024];
        let expected: [u8; 32] = Sha256::digest(&original).into();
        assert!(chunk_sha256_matches(&original, &expected));
        let mut corrupted = original;
        corrupted[32 * 1024] ^= 1;
        assert!(!chunk_sha256_matches(&corrupted, &expected));
    }

    #[test]
    fn discarded_lazy_page_is_zero_and_never_resurrected_by_chunk_prefetch() {
        const PAGE: usize = 4096;
        const LEN: usize = PAGE * 2;
        let path = std::env::temp_dir().join(format!(
            "tarit-uffd-discard-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let snapshot = vec![0x5a; LEN];
        file.write_all(&snapshot).unwrap();
        file.sync_all().unwrap();

        // SAFETY: this creates one private, page-aligned anonymous mapping,
        // checked against MAP_FAILED and unmapped exactly once below.
        let guest = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(guest, libc::MAP_FAILED);
        let hash: [u8; 32] = Sha256::digest(&snapshot).into();
        let lazy = match start_lazy_restore_with_integrity(
            guest.cast(),
            LEN,
            &file,
            0,
            LEN as u64,
            None,
            Some(ChunkIntegrity {
                chunk_size: LEN,
                chunk_hashes: vec![hash],
            }),
        ) {
            Ok(lazy) => lazy,
            Err(UffdRestoreError::Userfaultfd(error))
                if error.raw_os_error() == Some(libc::EPERM) =>
            {
                // Some CI kernels disable unprivileged userfaultfd. The real
                // UFFD path remains mandatory on the c8i promotion runner.
                // SAFETY: `guest` is the live mapping created above.
                assert_eq!(unsafe { libc::munmap(guest, LEN) }, 0);
                drop(file);
                std::fs::remove_file(path).unwrap();
                return;
            }
            Err(error) => panic!("start lazy restore: {error}"),
        };
        // Faulting page zero prefetches the authenticated two-page chunk, then
        // discard a now-resident page. MADV_DONTNEED queues UFFD_EVENT_REMOVE;
        // the following refault proves that the handler drains REMOVE, retries
        // any EAGAIN-deferred PAGEFAULT, and returns zero rather than hanging or
        // resurrecting snapshot bytes.
        // SAFETY: `guest` remains mapped and UFFD-registered for both reads.
        let first = unsafe { std::ptr::read_volatile(guest.cast::<u8>()) };
        lazy.page_discard().discard(PAGE, PAGE).unwrap();
        let second = unsafe { std::ptr::read_volatile(guest.cast::<u8>().add(PAGE)) };
        assert_eq!(first, 0x5a);
        assert_eq!(second, 0);

        drop(lazy);
        // SAFETY: `guest` is the live mapping returned above and has not yet
        // been unmapped.
        assert_eq!(unsafe { libc::munmap(guest, LEN) }, 0);
        drop(file);
        std::fs::remove_file(path).unwrap();
    }

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
