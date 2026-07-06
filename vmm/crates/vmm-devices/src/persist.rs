//! Persist trait — every device implements save/restore (PRD §1, §9a, §10).
//!
//! "Snapshot-native from day one. Memory layout, device state, and the run
//! loop are all designed around serialize/restore and dirty-page tracking,
//! not bolted on later." This trait is the contract every device satisfies.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::any::type_name;

/// A device whose internal state can be serialized to / restored from a
/// snapshot.
///
/// Implementors pick a `State` type (a serde Serialize/DeserializeOwned
/// struct). The snapshot layer (vmm-snapshot) collects `State` from every
/// registered device, CRCs the blob, and writes the state file.
pub trait Persist {
    type State: Serialize + DeserializeOwned;

    /// Human-readable type name for this device's state — used as the key
    /// in the state file. Defaults to Rust's type name.
    fn state_key(&self) -> &'static str {
        type_name::<Self::State>()
    }

    /// Serialize device state.
    fn save(&self) -> Self::State;

    /// Restore device state. Called on a freshly-constructed device.
    fn restore(&mut self, state: Self::State);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
    struct CounterState {
        n: u64,
    }

    struct Counter {
        n: u64,
    }
    impl Persist for Counter {
        type State = CounterState;
        fn save(&self) -> Self::State {
            CounterState { n: self.n }
        }
        fn restore(&mut self, state: Self::State) {
            self.n = state.n;
        }
    }

    #[test]
    fn round_trip() {
        let c = Counter { n: 42 };
        let s = c.save();
        let mut c2 = Counter { n: 0 };
        c2.restore(s);
        assert_eq!(c2.n, 42);
    }
}
