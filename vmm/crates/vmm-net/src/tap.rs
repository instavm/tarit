//! TAP interface — real host-side TAP device creation.
//!
//! Creates a TAP device using `/dev/net/tun` + `TUNSETIFF` ioctl (the
//! standard Linux TAP creation path). There's no rust-vmm crate for this;
//! it's raw syscalls via libc/nix.
//!
//! The TAP is moved into a per-VM netns by the orchestrator/jailer.
//! The VMM just opens it and wires it to the virtio-net device.

use std::ffi::CString;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TapError {
    #[error("open /dev/net/tun: {0}")]
    Open(String),
    #[error("ioctl TUNSETIFF: {0}")]
    Ioctl(String),
    #[error("ioctl TUNSETPERSIST: {0}")]
    Persist(String),
    #[error("set non-blocking: {0}")]
    Nonblock(String),
}

/// IFF_TAP | IFF_NO_PI flags for the TUN device.
const IFF_TAP: u16 = 0x0002;
const IFF_NO_PI: u16 = 0x1000;

/// TUNSETIFF ioctl request.
const TUNSETIFF: libc::c_ulong = 0x400454ca;
/// TUNSETPERSIST ioctl request.
const TUNSETPERSIST: libc::c_ulong = 0x400454cb;

/// A TAP device handle.
#[derive(Debug)]
pub struct Tap {
    pub fd: RawFd,
    pub name: String,
    closed: AtomicBool,
}

/// The `ifreq` struct for TUNSETIFF.
#[repr(C)]
struct Ifreq {
    name: [u8; 16],
    flags: u16,
    _pad: [u8; 22],
}

impl Tap {
    /// Create a new TAP device with the given name.
    /// If `name` is empty, the kernel assigns a name (tap0, tap1, ...).
    pub fn create(name: &str) -> Result<Self, TapError> {
        let path = CString::new("/dev/net/tun").unwrap();
        // SAFETY: `path` is a valid NUL-terminated C string and `open` does
        // not retain the pointer after the call returns.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(TapError::Open(format!(
                "errno: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Build the ifreq struct.
        let mut ifr = Ifreq {
            name: [0u8; 16],
            flags: IFF_TAP | IFF_NO_PI,
            _pad: [0u8; 22],
        };
        let name_bytes = name.as_bytes();
        let copy_len = name_bytes.len().min(15);
        ifr.name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        // TUNSETIFF.
        // SAFETY: `fd` is an open `/dev/net/tun` fd and `ifr` points to a
        // properly initialized `ifreq`-compatible buffer for the duration of
        // the ioctl call.
        let rc = unsafe { libc::ioctl(fd, TUNSETIFF as _, &mut ifr) };
        if rc < 0 {
            // SAFETY: `fd` was returned by `open` above and has not been
            // handed to `Tap`, so this error path owns the close.
            unsafe {
                libc::close(fd);
            }
            return Err(TapError::Ioctl(format!(
                "errno: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Read back the actual name (in case we passed "").
        let actual_name = {
            let end = ifr.name.iter().position(|&b| b == 0).unwrap_or(16);
            String::from_utf8_lossy(&ifr.name[..end]).to_string()
        };

        // Make it persistent (survives close).
        // SAFETY: `fd` is an open TAP fd; the TUNSETPERSIST ioctl expects an
        // integer flag value as its third argument.
        let rc = unsafe { libc::ioctl(fd, TUNSETPERSIST as _, 1) };
        if rc < 0 {
            // Non-fatal — the TAP still works, it's just not persistent.
            log::warn!("TUNSETPERSIST failed: {}", std::io::Error::last_os_error());
        }

        // Set non-blocking (for epoll-based I/O).
        // SAFETY: `fd` is an open file descriptor; F_GETFL does not require an
        // additional pointer argument.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags >= 0 {
            // SAFETY: `fd` is open and `flags | O_NONBLOCK` is a valid file
            // status flag set for F_SETFL.
            unsafe {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }

        log::info!("TAP created: {actual_name} (fd={fd})");
        Ok(Self {
            fd,
            name: actual_name,
            closed: AtomicBool::new(false),
        })
    }

    /// Close the TAP device.
    pub fn close(&self) {
        if self.fd >= 0 && !self.closed.swap(true, Ordering::AcqRel) {
            // SAFETY: this `Tap` owns `fd`, and the atomic `closed` flag
            // ensures the descriptor is closed at most once by this object.
            unsafe {
                libc::close(self.fd);
            }
        }
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_error_display() {
        let e = TapError::Open("test".into());
        assert!(e.to_string().contains("/dev/net/tun"));
        let e = TapError::Ioctl("EBUSY".into());
        assert!(e.to_string().contains("TUNSETIFF"));
    }
}
