//! virtio-net device — tap-backed NIC over virtio-mmio (PRD §8).
//!
//! PRD §8: "Each microVM gets a `virtio-net` device backed by a host tap
//! interface, and that tap lives inside a dedicated network namespace per
//! VM. Egress is enforced host-side (vmm-net crate) so the guest cannot
//! bypass it."
//!
//! This module owns the device-side state (MAC, queue handles). The host
//! tap + netns + egress policy lives in `vmm-net`. The I/O loop that moves
//! packets between the virtqueue and the tap fd lives in [`net_io_loop`]
//! (event-manager, Linux-only).

use crate::persist::Persist;
use serde::{Deserialize, Serialize};

/// Default MAC assigned to a microVM NIC when none is configured
/// (Firecracker-style locally-administered, unicast).
pub const DEFAULT_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];

/// virtio-net feature bits the device advertises (virtio 1.x §5.1.5).
pub mod features {
    pub const CSUM: u32 = 1 << 0;
    pub const GUEST_CSUM: u32 = 1 << 2;
    pub const MAC: u32 = 1 << 5;
    pub const EVENT_IDX: u32 = 1 << 29;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NetState {
    pub mac: [u8; 6],
}

pub struct VirtioNet {
    pub mac: [u8; 6],
    pub tap_name: String,
    /// Host-side features advertised to the guest.
    pub features: u32,
}

impl VirtioNet {
    pub fn new(tap_name: String, mac: Option<[u8; 6]>) -> Self {
        Self {
            mac: mac.unwrap_or(DEFAULT_MAC),
            tap_name,
            features: features::MAC, // we always expose a MAC
        }
    }
}

impl Persist for VirtioNet {
    type State = NetState;
    fn save(&self) -> Self::State {
        NetState { mac: self.mac }
    }
    fn restore(&mut self, state: Self::State) {
        self.mac = state.mac;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mac_is_local_unicast() {
        // First byte's low bit (unicast/multicast) is 0, second-high bit
        // (local/global) is 1 → locally-administered unicast.
        assert_eq!(DEFAULT_MAC[0] & 0b11, 0b10);
    }

    #[test]
    fn new_uses_provided_mac_or_default() {
        let n = VirtioNet::new("tap0".into(), None);
        assert_eq!(n.mac, DEFAULT_MAC);
        let n = VirtioNet::new("tap0".into(), Some([0xAA; 6]));
        assert_eq!(n.mac, [0xAA; 6]);
    }

    #[test]
    fn persist_round_trip() {
        let n = VirtioNet::new("tap0".into(), Some([1, 2, 3, 4, 5, 6]));
        let st = n.save();
        let mut n2 = VirtioNet::new("tap1".into(), None);
        n2.restore(st);
        assert_eq!(n2.mac, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn features_include_mac() {
        let n = VirtioNet::new("tap0".into(), None);
        assert_ne!(n.features & features::MAC, 0);
    }
}
