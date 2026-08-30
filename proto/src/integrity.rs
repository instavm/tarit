//! Durable chunk-integrity manifest shared by the orchestrator and VMM.
//!
//! The manifest itself is authenticated by a SHA-256 digest stored in trusted
//! control-plane metadata. RAM chunks are then verified only when UFFD faults
//! them in, avoiding a size-proportional read on the restore critical path.

use std::fmt;

pub const INTEGRITY_CHUNK_SIZE: u32 = 64 * 1024;
const MAGIC: &[u8; 4] = b"TIMF";
const VERSION: u16 = 1;
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
        let mut output = Vec::new();
        output.extend_from_slice(MAGIC);
        output.extend_from_slice(&VERSION.to_le_bytes());
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
        Ok(output)
    }

    pub fn decode(input: &[u8]) -> Result<Self, IntegrityManifestError> {
        if input.len() < HEADER_LEN || &input[..4] != MAGIC {
            return Err(IntegrityManifestError(
                "bad integrity manifest header".into(),
            ));
        }
        let version = u16::from_le_bytes(input[4..6].try_into().expect("two-byte version"));
        if version != VERSION {
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
            for hash in encoded_hashes.chunks_exact(HASH_LEN) {
                chunk_hashes.push(hash.try_into().expect("32-byte hash"));
            }
            artifacts.push(ArtifactIntegrity {
                kind,
                len,
                chunk_hashes,
            });
            cursor = hashes_end;
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
        };
        let bytes = manifest.encode().unwrap();
        assert_eq!(IntegrityManifest::decode(&bytes).unwrap(), manifest);
        let mut trailing = bytes;
        trailing.push(0);
        assert!(IntegrityManifest::decode(&trailing).is_err());
    }
}
