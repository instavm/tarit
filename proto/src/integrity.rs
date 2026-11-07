//! Durable chunk-integrity manifest shared by the orchestrator and VMM.
//!
//! The manifest itself is authenticated by a SHA-256 digest stored in trusted
//! control-plane metadata. RAM chunks are then verified only when UFFD faults
//! them in, avoiding a size-proportional read on the restore critical path.
//!
//! Version 2 adds an optional working set: the RAM chunks the snapshotted
//! guest touched after its own restore, which a restore of this snapshot
//! loads ahead of demand. A manifest without one is still written as
//! version 1, so older readers keep accepting it.

use std::fmt;

pub const INTEGRITY_CHUNK_SIZE: u32 = 64 * 1024;
const MAGIC: &[u8; 4] = b"TIMF";
const VERSION_1: u16 = 1;
const VERSION_2: u16 = 2;
const HEADER_LEN: usize = 12;
const ARTIFACT_HEADER_LEN: usize = 24;
const HASH_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ArtifactKind {
    Ram = 1,
    Overlay = 2,
    SnapshotMetadata = 3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactIntegrity {
    pub kind: ArtifactKind,
    pub len: u64,
    pub chunk_hashes: Vec<[u8; HASH_LEN]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityManifest {
    pub chunk_size: u32,
    pub artifacts: Vec<ArtifactIntegrity>,
    /// Indices of RAM chunks to load ahead of demand on restore, ascending
    /// and unique. Only a hint: every chunk is still verified against its
    /// hash before the guest can see it.
    pub working_set: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityManifestError(String);

impl fmt::Display for IntegrityManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for IntegrityManifestError {}

impl IntegrityManifest {
    pub fn encode(&self) -> Result<Vec<u8>, IntegrityManifestError> {
        validate_chunk_size(self.chunk_size)?;
        if self.artifacts.is_empty() || self.artifacts.len() > u16::MAX as usize {
            return Err(IntegrityManifestError("invalid artifact count".into()));
        }
        let working_set = self.working_set_bitmap()?;
        let mut output = Vec::new();
        output.extend_from_slice(MAGIC);
        let version = if working_set.is_some() {
            VERSION_2
        } else {
            VERSION_1
        };
        output.extend_from_slice(&version.to_le_bytes());
        output.extend_from_slice(&(self.artifacts.len() as u16).to_le_bytes());
        output.extend_from_slice(&self.chunk_size.to_le_bytes());
        for artifact in &self.artifacts {
            let expected = chunk_count(artifact.len, self.chunk_size)?;
            if artifact.chunk_hashes.len() != expected {
                return Err(IntegrityManifestError(format!(
                    "artifact {:?} has {} hashes, expected {expected}",
                    artifact.kind,
                    artifact.chunk_hashes.len()
                )));
            }
            output.push(artifact.kind as u8);
            output.extend_from_slice(&[0; 7]);
            output.extend_from_slice(&artifact.len.to_le_bytes());
            output.extend_from_slice(&(expected as u64).to_le_bytes());
            for hash in &artifact.chunk_hashes {
                output.extend_from_slice(hash);
            }
        }
        if let Some((chunks, bitmap)) = working_set {
            output.extend_from_slice(&(chunks as u64).to_le_bytes());
            output.extend_from_slice(&bitmap);
        }
        Ok(output)
    }

    /// The working set as (RAM chunk count, bitmap), or `None` when empty.
    fn working_set_bitmap(&self) -> Result<Option<(usize, Vec<u8>)>, IntegrityManifestError> {
        if self.working_set.is_empty() {
            return Ok(None);
        }
        let chunks = self
            .artifact(ArtifactKind::Ram)
            .ok_or_else(|| IntegrityManifestError("working set without a RAM artifact".into()))?
            .chunk_hashes
            .len();
        let mut bitmap = vec![0u8; chunks.div_ceil(8)];
        let mut previous = None;
        for &chunk in &self.working_set {
            let index = chunk as usize;
            if index >= chunks || previous.is_some_and(|previous| previous >= chunk) {
                return Err(IntegrityManifestError(
                    "working set must list ascending RAM chunk indices".into(),
                ));
            }
            previous = Some(chunk);
            bitmap[index / 8] |= 1 << (index % 8);
        }
        Ok(Some((chunks, bitmap)))
    }

    pub fn decode(input: &[u8]) -> Result<Self, IntegrityManifestError> {
        if input.len() < HEADER_LEN || &input[..4] != MAGIC {
            return Err(IntegrityManifestError(
                "bad integrity manifest header".into(),
            ));
        }
        let version = u16::from_le_bytes(input[4..6].try_into().expect("two-byte version"));
        if version != VERSION_1 && version != VERSION_2 {
            return Err(IntegrityManifestError(format!(
                "unsupported integrity manifest version {version}"
            )));
        }
        let artifact_count =
            u16::from_le_bytes(input[6..8].try_into().expect("two-byte count")) as usize;
        if artifact_count == 0 {
            return Err(IntegrityManifestError("empty integrity manifest".into()));
        }
        let chunk_size = u32::from_le_bytes(input[8..12].try_into().expect("four-byte size"));
        validate_chunk_size(chunk_size)?;
        let mut cursor = HEADER_LEN;
        let mut artifacts = Vec::with_capacity(artifact_count);
        for _ in 0..artifact_count {
            let header_end = cursor
                .checked_add(ARTIFACT_HEADER_LEN)
                .ok_or_else(|| IntegrityManifestError("manifest offset overflow".into()))?;
            let header = input
                .get(cursor..header_end)
                .ok_or_else(|| IntegrityManifestError("truncated artifact header".into()))?;
            let kind = match header[0] {
                1 => ArtifactKind::Ram,
                2 => ArtifactKind::Overlay,
                3 => ArtifactKind::SnapshotMetadata,
                other => {
                    return Err(IntegrityManifestError(format!(
                        "unknown artifact kind {other}"
                    )))
                }
            };
            if header[1..8].iter().any(|byte| *byte != 0) {
                return Err(IntegrityManifestError("non-zero reserved bytes".into()));
            }
            let len = u64::from_le_bytes(header[8..16].try_into().expect("eight-byte length"));
            let encoded_count =
                u64::from_le_bytes(header[16..24].try_into().expect("eight-byte count"));
            let expected_count = chunk_count(len, chunk_size)?;
            if encoded_count != expected_count as u64 {
                return Err(IntegrityManifestError(
                    "invalid artifact chunk count".into(),
                ));
            }
            cursor = header_end;
            let hashes_len = expected_count
                .checked_mul(HASH_LEN)
                .ok_or_else(|| IntegrityManifestError("manifest hash length overflow".into()))?;
            let hashes_end = cursor
                .checked_add(hashes_len)
                .ok_or_else(|| IntegrityManifestError("manifest offset overflow".into()))?;
            let encoded_hashes = input
                .get(cursor..hashes_end)
                .ok_or_else(|| IntegrityManifestError("truncated chunk hashes".into()))?;
            let mut chunk_hashes = Vec::with_capacity(expected_count);
            for hash in encoded_hashes.chunks(HASH_LEN) {
                chunk_hashes.push(hash.try_into().expect("32-byte hash"));
            }
            artifacts.push(ArtifactIntegrity {
                kind,
                len,
                chunk_hashes,
            });
            cursor = hashes_end;
        }
        let mut working_set = Vec::new();
        if version == VERSION_2 {
            let ram_chunks = artifacts
                .iter()
                .find(|artifact| artifact.kind == ArtifactKind::Ram)
                .map_or(0, |artifact| artifact.chunk_hashes.len());
            let count_end = cursor
                .checked_add(8)
                .ok_or_else(|| IntegrityManifestError("manifest offset overflow".into()))?;
            let count = input
                .get(cursor..count_end)
                .ok_or_else(|| IntegrityManifestError("truncated working set".into()))?;
            let count = u64::from_le_bytes(count.try_into().expect("eight-byte count"));
            if count != ram_chunks as u64 || ram_chunks == 0 {
                return Err(IntegrityManifestError(
                    "working set does not match the RAM chunk count".into(),
                ));
            }
            let bitmap_end = count_end
                .checked_add(ram_chunks.div_ceil(8))
                .ok_or_else(|| IntegrityManifestError("manifest offset overflow".into()))?;
            let bitmap = input
                .get(count_end..bitmap_end)
                .ok_or_else(|| IntegrityManifestError("truncated working set".into()))?;
            for (byte_index, byte) in bitmap.iter().enumerate() {
                for bit in 0..8 {
                    if byte & (1 << bit) == 0 {
                        continue;
                    }
                    let index = byte_index * 8 + bit;
                    if index >= ram_chunks {
                        return Err(IntegrityManifestError(
                            "working set names a chunk past the end of RAM".into(),
                        ));
                    }
                    working_set.push(u32::try_from(index).map_err(|_| {
                        IntegrityManifestError("working set chunk index too large".into())
                    })?);
                }
            }
            if working_set.is_empty() {
                return Err(IntegrityManifestError(
                    "version 2 manifest with an empty working set".into(),
                ));
            }
            cursor = bitmap_end;
        }
        if cursor != input.len() {
            return Err(IntegrityManifestError(
                "trailing integrity manifest data".into(),
            ));
        }
        if artifacts
            .iter()
            .filter(|a| a.kind == ArtifactKind::Ram)
            .count()
            != 1
            || artifacts
                .iter()
                .filter(|a| a.kind == ArtifactKind::SnapshotMetadata)
                .count()
                != 1
            || artifacts
                .iter()
                .filter(|a| a.kind == ArtifactKind::Overlay)
                .count()
                > 1
        {
            return Err(IntegrityManifestError(
                "manifest must contain exactly one RAM and at most one overlay artifact".into(),
            ));
        }
        Ok(Self {
            chunk_size,
            artifacts,
            working_set,
        })
    }

    pub fn artifact(&self, kind: ArtifactKind) -> Option<&ArtifactIntegrity> {
        self.artifacts.iter().find(|artifact| artifact.kind == kind)
    }
}

fn validate_chunk_size(chunk_size: u32) -> Result<(), IntegrityManifestError> {
    if chunk_size < 4096 || !chunk_size.is_power_of_two() || chunk_size > 4 * 1024 * 1024 {
        return Err(IntegrityManifestError(
            "invalid integrity chunk size".into(),
        ));
    }
    Ok(())
}

fn chunk_count(len: u64, chunk_size: u32) -> Result<usize, IntegrityManifestError> {
    if len == 0 {
        return Ok(0);
    }
    let count = len
        .checked_add(u64::from(chunk_size) - 1)
        .ok_or_else(|| IntegrityManifestError("artifact length overflow".into()))?
        / u64::from(chunk_size);
    usize::try_from(count).map_err(|_| IntegrityManifestError("chunk count too large".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trip_and_rejects_trailing_data() {
        let manifest = IntegrityManifest {
            chunk_size: INTEGRITY_CHUNK_SIZE,
            artifacts: vec![
                ArtifactIntegrity {
                    kind: ArtifactKind::SnapshotMetadata,
                    len: 1,
                    chunk_hashes: vec![[0; 32]],
                },
                ArtifactIntegrity {
                    kind: ArtifactKind::Ram,
                    len: u64::from(INTEGRITY_CHUNK_SIZE) + 1,
                    chunk_hashes: vec![[1; 32], [2; 32]],
                },
            ],
            working_set: Vec::new(),
        };
        let bytes = manifest.encode().unwrap();
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), VERSION_1);
        assert_eq!(IntegrityManifest::decode(&bytes).unwrap(), manifest);
        let mut trailing = bytes;
        trailing.push(0);
        assert!(IntegrityManifest::decode(&trailing).is_err());
    }

    fn manifest_with_ram_chunks(chunks: usize, working_set: Vec<u32>) -> IntegrityManifest {
        IntegrityManifest {
            chunk_size: INTEGRITY_CHUNK_SIZE,
            artifacts: vec![
                ArtifactIntegrity {
                    kind: ArtifactKind::SnapshotMetadata,
                    len: 1,
                    chunk_hashes: vec![[0; 32]],
                },
                ArtifactIntegrity {
                    kind: ArtifactKind::Ram,
                    len: u64::from(INTEGRITY_CHUNK_SIZE) * chunks as u64,
                    chunk_hashes: vec![[7; 32]; chunks],
                },
            ],
            working_set,
        }
    }

    #[test]
    fn working_set_round_trips_as_version_2() {
        let manifest = manifest_with_ram_chunks(19, vec![0, 3, 8, 18]);
        let bytes = manifest.encode().unwrap();
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), VERSION_2);
        assert_eq!(IntegrityManifest::decode(&bytes).unwrap(), manifest);
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(IntegrityManifest::decode(&trailing).is_err());
        let mut truncated = bytes;
        truncated.pop();
        assert!(IntegrityManifest::decode(&truncated).is_err());
    }

    #[test]
    fn working_set_must_name_real_chunks_in_order() {
        for bad in [vec![19], vec![4, 2], vec![5, 5]] {
            assert!(manifest_with_ram_chunks(19, bad).encode().is_err());
        }
        // A padding bit past the last chunk is rejected on decode.
        let mut bytes = manifest_with_ram_chunks(19, vec![1]).encode().unwrap();
        *bytes.last_mut().unwrap() |= 0x80;
        assert!(IntegrityManifest::decode(&bytes).is_err());
        // So is a version 2 manifest whose working set is empty.
        let last = bytes.len() - 3;
        bytes[last..].fill(0);
        assert!(IntegrityManifest::decode(&bytes).is_err());
    }
}
