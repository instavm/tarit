//! virtio-rng — entropy source for the guest (PRD §13 risk: PRNG state).
//!
//! When a VM is snapshotted and restored/cloned, the guest's PRNG state is
//! captured in the memory snapshot. But if 100 clones share the same PRNG
//! state, they'll all produce the same random numbers — a security risk.
//!
//! virtio-rng solves this: the guest pulls fresh entropy from the host's
//! /dev/urandom on each restore/clone. The kernel's CRNG re-seeds from
//! virtio-rng, so each clone gets independent randomness even though they
//! started from the same memory snapshot.
//!
//! Implementation: the guest requests entropy via a virtqueue; we fill the
//! buffer from the host's /dev/urandom. Simple, secure, no state to manage.

use crate::persist::Persist;
use serde::{Deserialize, Serialize};

/// virtio-rng device ID.
pub const DEVICE_ID_RNG: u32 = 4;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RngState {
    pub bytes_served: u64,
}

/// A virtio-rng device backed by the host kernel CSPRNG.
pub struct VirtioRng {
    pub bytes_served: u64,
}

impl Default for VirtioRng {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtioRng {
    pub fn new() -> Self {
        Self { bytes_served: 0 }
    }

    /// Fill `buf` with random bytes from the host kernel CSPRNG.
    /// Called when the guest submits an "entropy request" on the virtqueue.
    pub fn fill_entropy(&mut self, buf: &mut [u8]) -> Result<(), std::io::Error> {
        fill_host_entropy(buf)?;
        self.bytes_served += buf.len() as u64;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn fill_host_entropy(mut buf: &mut [u8]) -> Result<(), std::io::Error> {
    use std::io::{Error, ErrorKind};

    while !buf.is_empty() {
        // SAFETY: `buf` is a valid writable slice for `buf.len()` bytes, and
        // getrandom only writes to that buffer without retaining the pointer.
        let ret = unsafe { libc::getrandom(buf.as_mut_ptr().cast::<libc::c_void>(), buf.len(), 0) };
        if ret < 0 {
            let err = Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if ret == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "getrandom returned 0"));
        }
        let n = ret as usize;
        let (_, rest) = buf.split_at_mut(n);
        buf = rest;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn fill_host_entropy(buf: &mut [u8]) -> Result<(), std::io::Error> {
    use std::io::Read;

    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(buf)
}

impl Persist for VirtioRng {
    type State = RngState;
    fn save(&self) -> Self::State {
        RngState {
            bytes_served: self.bytes_served,
        }
    }
    fn restore(&mut self, state: Self::State) {
        // On restore/clone, DON'T restore bytes_served — each clone starts
        // fresh. The important thing is that the guest pulls NEW entropy
        // from the host after restore, re-seeding its CRNG.
        self.bytes_served = 0;
        let _ = state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_entropy_produces_nonzero_bytes() {
        let mut rng = VirtioRng::new();
        let mut buf = [0u8; 32];
        rng.fill_entropy(&mut buf).unwrap();
        // At least some bytes should be non-zero (probabilistically certain
        // for 32 bytes from /dev/urandom, but we check a few).
        assert!(buf.iter().any(|&b| b != 0));
        assert_eq!(rng.bytes_served, 32);
    }

    #[test]
    fn restore_resets_byte_count() {
        let mut rng = VirtioRng::new();
        rng.bytes_served = 999;
        rng.restore(RngState { bytes_served: 999 });
        assert_eq!(
            rng.bytes_served, 0,
            "clones must start with fresh entropy count"
        );
    }

    #[test]
    fn two_fill_calls_produce_different_output() {
        let mut rng = VirtioRng::new();
        let mut buf1 = [0u8; 16];
        let mut buf2 = [0u8; 16];
        rng.fill_entropy(&mut buf1).unwrap();
        rng.fill_entropy(&mut buf2).unwrap();
        assert_ne!(buf1, buf2, "consecutive entropy pulls must differ");
    }
}
