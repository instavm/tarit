//! Device state serialization — the "state file" half of a snapshot
//! (PRD §9a: "serialize device state ... into a small state file").
//!
//! Every device implements `Persist`; the collector calls `save()` on each,
//! tags each entry with a stable key + schema version, and postcard-encodes
//! the whole into a `Vec<u8>` that the snapshot layer CRCs and writes.
//!
//! On restore, the collector walks the entries and calls
//! `restore(state)` on each matching device.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;
use vmm_devices::persist::Persist;

/// Schema version for the device-state blob. Bumped when any device's
/// `State` shape changes; restore must reject a mismatched version (PRD §13:
/// "version the device `Persist` schema").
pub const SCHEMA_VERSION: u16 = 1;
pub const MAX_DEVICE_STATE_BLOB_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_DEVICE_STATE_ENTRIES: usize = 1024;
pub const MAX_DEVICE_STATE_KEY_BYTES: usize = 256;
pub const MAX_DEVICE_STATE_ENTRY_BYTES: usize = 16 * 1024 * 1024;

/// One device's serialized state, tagged with its key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceStateEntry {
    pub key: String,
    /// Postcard of the device's `Persist::State`.
    pub bytes: Vec<u8>,
}

/// The full device-state blob: schema version + a map of key → bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceStateBlob {
    pub schema_version: u16,
    pub entries: BTreeMap<String, Vec<u8>>,
}

impl DeviceStateBlob {
    /// Build a new, empty blob.
    pub fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    }

    /// Collect `save()` from every device into a single blob.
    pub fn collect<I, D>(devices: I) -> Result<Self, StateError>
    where
        I: IntoIterator<Item = D>,
        D: Persist,
        D::State: Serialize + DeserializeOwned,
    {
        let mut blob = Self::new();
        for dev in devices {
            let state = dev.save();
            let bytes = postcard::to_allocvec(&state)
                .map_err(|e| StateError::Encode(format!("{}: {e}", dev.state_key())))?;
            blob.entries.insert(dev.state_key().to_string(), bytes);
        }
        blob.validate_bounds()?;
        Ok(blob)
    }

    /// Encode the blob to bytes (postcard).
    pub fn to_bytes(&self) -> Result<Vec<u8>, StateError> {
        self.validate_bounds()?;
        let bytes = postcard::to_allocvec(self).map_err(|e| StateError::Encode(e.to_string()))?;
        if bytes.len() > MAX_DEVICE_STATE_BLOB_BYTES {
            return Err(StateError::BlobTooLarge {
                len: bytes.len(),
                max: MAX_DEVICE_STATE_BLOB_BYTES,
            });
        }
        Ok(bytes)
    }

    /// Decode a blob from bytes, verifying the schema version.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, StateError> {
        if bytes.len() > MAX_DEVICE_STATE_BLOB_BYTES {
            return Err(StateError::BlobTooLarge {
                len: bytes.len(),
                max: MAX_DEVICE_STATE_BLOB_BYTES,
            });
        }
        let blob: DeviceStateBlob =
            postcard::from_bytes(bytes).map_err(|e| StateError::Decode(e.to_string()))?;
        if blob.schema_version != SCHEMA_VERSION {
            return Err(StateError::SchemaMismatch {
                found: blob.schema_version,
                expected: SCHEMA_VERSION,
            });
        }
        blob.validate_bounds()?;
        Ok(blob)
    }

    /// Look up the serialized state for a device key.
    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.entries.get(key).map(|v| v.as_slice())
    }

    fn validate_bounds(&self) -> Result<(), StateError> {
        if self.entries.len() > MAX_DEVICE_STATE_ENTRIES {
            return Err(StateError::TooManyEntries {
                len: self.entries.len(),
                max: MAX_DEVICE_STATE_ENTRIES,
            });
        }
        for (key, value) in &self.entries {
            if key.len() > MAX_DEVICE_STATE_KEY_BYTES {
                return Err(StateError::KeyTooLarge {
                    key: key.clone(),
                    len: key.len(),
                    max: MAX_DEVICE_STATE_KEY_BYTES,
                });
            }
            if value.len() > MAX_DEVICE_STATE_ENTRY_BYTES {
                return Err(StateError::EntryTooLarge {
                    key: key.clone(),
                    len: value.len(),
                    max: MAX_DEVICE_STATE_ENTRY_BYTES,
                });
            }
        }
        Ok(())
    }
}

impl Default for DeviceStateBlob {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("encode: {0}")]
    Encode(String),
    #[error("decode: {0}")]
    Decode(String),
    #[error("device state blob too large: {len} > {max}")]
    BlobTooLarge { len: usize, max: usize },
    #[error("too many device state entries: {len} > {max}")]
    TooManyEntries { len: usize, max: usize },
    #[error("device state key {key:?} too large: {len} > {max}")]
    KeyTooLarge { key: String, len: usize, max: usize },
    #[error("device state entry {key:?} too large: {len} > {max}")]
    EntryTooLarge { key: String, len: usize, max: usize },
    #[error("schema mismatch: found {found}, expected {expected}")]
    SchemaMismatch { found: u16, expected: u16 },
    #[error("device {0} missing from blob")]
    Missing(String),
    #[error("device {0} failed to restore: {1}")]
    Restore(String, String),
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
    fn collect_and_round_trip() {
        // Collect a single Counter (Persist::state_key returns the type name,
        // so two Counters collide on the key — real devices have distinct
        // State types → distinct keys). Round-trip the blob.
        let blob = DeviceStateBlob::collect([Counter { n: 7 }]).unwrap();
        let bytes = blob.to_bytes().unwrap();
        let back = DeviceStateBlob::from_bytes(&bytes).unwrap();
        assert_eq!(back, blob);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn schema_mismatch_rejected() {
        // Hand-craft a blob with the wrong schema version.
        let mut entries = BTreeMap::new();
        entries.insert("k".to_string(), vec![1u8]);
        let bad = DeviceStateBlob {
            schema_version: 999,
            entries,
        };
        let bytes = postcard::to_allocvec(&bad).unwrap();
        assert!(matches!(
            DeviceStateBlob::from_bytes(&bytes),
            Err(StateError::SchemaMismatch { found: 999, .. })
        ));
    }

    #[test]
    fn empty_blob_round_trips() {
        let blob = DeviceStateBlob::new();
        let bytes = blob.to_bytes().unwrap();
        let back = DeviceStateBlob::from_bytes(&bytes).unwrap();
        assert!(back.entries.is_empty());
    }

    #[test]
    fn oversized_blob_rejected_before_deserialize() {
        let bytes = vec![0u8; MAX_DEVICE_STATE_BLOB_BYTES + 1];
        assert!(matches!(
            DeviceStateBlob::from_bytes(&bytes),
            Err(StateError::BlobTooLarge { .. })
        ));
    }

    #[test]
    fn oversized_entry_rejected_before_serialize() {
        let mut blob = DeviceStateBlob::new();
        blob.entries.insert(
            "large".to_string(),
            vec![0u8; MAX_DEVICE_STATE_ENTRY_BYTES + 1],
        );
        assert!(matches!(
            blob.to_bytes(),
            Err(StateError::EntryTooLarge { .. })
        ));
    }
}
