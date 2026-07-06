//! Migration negotiation — source/destination agree on protocol version,
//! CPU feature set (CPUID/MSR template compatibility), device model, RAM
//! size, and volume identity over an authenticated mTLS control channel.
//! Scaffold (M15).

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

pub const SUPPORTED_PROTOCOL_VERSION: u16 = 1;
pub const MAX_VMM_VERSION_BYTES: usize = 64;
pub const MAX_CPU_TEMPLATE_BYTES: usize = 128;
pub const MAX_VOLUME_IDS: usize = 64;
pub const MAX_VOLUME_ID_BYTES: usize = 256;
pub const MAX_REJECT_REASON_BYTES: usize = 1024;
pub const MIN_RAM_MIB: u64 = 1;
pub const MAX_RAM_MIB: u64 = 1024 * 1024; // 1 TiB
pub const MIN_VCPU_COUNT: u8 = 1;
pub const MAX_VCPU_COUNT: u8 = 240;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NegotiationRequest {
    #[serde(deserialize_with = "deserialize_protocol_version")]
    pub protocol_version: u16,
    #[serde(deserialize_with = "deserialize_vmm_version")]
    pub vmm_version: String,
    #[serde(deserialize_with = "deserialize_cpu_template")]
    pub cpu_template: String,
    #[serde(deserialize_with = "deserialize_ram_mib")]
    pub ram_mib: u64,
    #[serde(deserialize_with = "deserialize_vcpu_count")]
    pub vcpu_count: u8,
    pub device_model_hash: [u8; 32],
    #[serde(deserialize_with = "deserialize_volume_ids")]
    pub volume_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NegotiationResponse {
    Accept,
    Reject(#[serde(deserialize_with = "deserialize_reject_reason")] String),
}

impl NegotiationRequest {
    pub fn validate(&self) -> Result<(), NegotiationError> {
        if self.protocol_version != SUPPORTED_PROTOCOL_VERSION {
            return Err(NegotiationError::UnsupportedProtocolVersion(
                self.protocol_version,
            ));
        }
        validate_string_len("vmm_version", &self.vmm_version, MAX_VMM_VERSION_BYTES)?;
        validate_string_len("cpu_template", &self.cpu_template, MAX_CPU_TEMPLATE_BYTES)?;
        if !(MIN_RAM_MIB..=MAX_RAM_MIB).contains(&self.ram_mib) {
            return Err(NegotiationError::InvalidRamMiB(self.ram_mib));
        }
        if !(MIN_VCPU_COUNT..=MAX_VCPU_COUNT).contains(&self.vcpu_count) {
            return Err(NegotiationError::InvalidVcpuCount(self.vcpu_count));
        }
        if self.volume_ids.len() > MAX_VOLUME_IDS {
            return Err(NegotiationError::TooManyVolumeIds {
                len: self.volume_ids.len(),
                max: MAX_VOLUME_IDS,
            });
        }
        for volume_id in &self.volume_ids {
            validate_string_len("volume_id", volume_id, MAX_VOLUME_ID_BYTES)?;
        }
        Ok(())
    }
}

impl NegotiationResponse {
    pub fn validate(&self) -> Result<(), NegotiationError> {
        if let NegotiationResponse::Reject(reason) = self {
            validate_string_len("reject_reason", reason, MAX_REJECT_REASON_BYTES)?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NegotiationError {
    #[error("unsupported protocol version {0}")]
    UnsupportedProtocolVersion(u16),
    #[error("{field} too long: {len} > {max}")]
    StringTooLong {
        field: &'static str,
        len: usize,
        max: usize,
    },
    #[error("ram_mib out of range: {0}")]
    InvalidRamMiB(u64),
    #[error("vcpu_count out of range: {0}")]
    InvalidVcpuCount(u8),
    #[error("too many volume IDs: {len} > {max}")]
    TooManyVolumeIds { len: usize, max: usize },
}

fn validate_string_len(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), NegotiationError> {
    if value.len() > max {
        return Err(NegotiationError::StringTooLong {
            field,
            len: value.len(),
            max,
        });
    }
    Ok(())
}

fn deserialize_protocol_version<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: Deserializer<'de>,
{
    let version = u16::deserialize(deserializer)?;
    if version != SUPPORTED_PROTOCOL_VERSION {
        return Err(de::Error::custom(format!(
            "unsupported protocol version {version}"
        )));
    }
    Ok(version)
}

fn deserialize_vmm_version<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_string(deserializer, "vmm_version", MAX_VMM_VERSION_BYTES)
}

fn deserialize_cpu_template<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_string(deserializer, "cpu_template", MAX_CPU_TEMPLATE_BYTES)
}

fn deserialize_reject_reason<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bounded_string(deserializer, "reject_reason", MAX_REJECT_REASON_BYTES)
}

fn deserialize_bounded_string<'de, D>(
    deserializer: D,
    field: &'static str,
    max: usize,
) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.len() > max {
        return Err(de::Error::custom(format!(
            "{field} too long: {} > {max}",
            value.len()
        )));
    }
    Ok(value)
}

fn deserialize_ram_mib<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let ram_mib = u64::deserialize(deserializer)?;
    if !(MIN_RAM_MIB..=MAX_RAM_MIB).contains(&ram_mib) {
        return Err(de::Error::custom(format!(
            "ram_mib out of range: {ram_mib}"
        )));
    }
    Ok(ram_mib)
}

fn deserialize_vcpu_count<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: Deserializer<'de>,
{
    let vcpu_count = u8::deserialize(deserializer)?;
    if !(MIN_VCPU_COUNT..=MAX_VCPU_COUNT).contains(&vcpu_count) {
        return Err(de::Error::custom(format!(
            "vcpu_count out of range: {vcpu_count}"
        )));
    }
    Ok(vcpu_count)
}

fn deserialize_volume_ids<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct VolumeIdsVisitor;

    impl<'de> Visitor<'de> for VolumeIdsVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "at most {MAX_VOLUME_IDS} bounded volume IDs")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            if let Some(hint) = seq.size_hint() {
                if hint > MAX_VOLUME_IDS {
                    return Err(de::Error::custom(format!(
                        "too many volume IDs: {hint} > {MAX_VOLUME_IDS}"
                    )));
                }
            }

            let mut volume_ids = Vec::new();
            while let Some(volume_id) = seq.next_element::<String>()? {
                if volume_ids.len() == MAX_VOLUME_IDS {
                    return Err(de::Error::custom(format!(
                        "too many volume IDs: more than {MAX_VOLUME_IDS}"
                    )));
                }
                if volume_id.len() > MAX_VOLUME_ID_BYTES {
                    return Err(de::Error::custom(format!(
                        "volume_id too long: {} > {MAX_VOLUME_ID_BYTES}",
                        volume_id.len()
                    )));
                }
                volume_ids.push(volume_id);
            }
            Ok(volume_ids)
        }
    }

    deserializer.deserialize_seq(VolumeIdsVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_json() {
        let r = NegotiationRequest {
            protocol_version: 1,
            vmm_version: "0.1.0".into(),
            cpu_template: "bare".into(),
            ram_mib: 256,
            vcpu_count: 1,
            device_model_hash: [0xAB; 32],
            volume_ids: vec!["vol-1".into(), "vol-2".into()],
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: NegotiationRequest = serde_json::from_str(&s).unwrap();
        back.validate().unwrap();
        assert_eq!(back.protocol_version, 1);
        assert_eq!(back.ram_mib, 256);
        assert_eq!(back.vcpu_count, 1);
        assert_eq!(back.volume_ids.len(), 2);
        assert_eq!(back.device_model_hash, [0xAB; 32]);
    }

    #[test]
    fn response_accept_round_trips() {
        let r = NegotiationResponse::Accept;
        let s = serde_json::to_string(&r).unwrap();
        let back: NegotiationResponse = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, NegotiationResponse::Accept));
    }

    #[test]
    fn response_reject_round_trips() {
        let r = NegotiationResponse::Reject("cpu mismatch".into());
        let s = serde_json::to_string(&r).unwrap();
        let back: NegotiationResponse = serde_json::from_str(&s).unwrap();
        back.validate().unwrap();
        assert!(matches!(back, NegotiationResponse::Reject(_)));
    }

    #[test]
    fn request_rejects_invalid_version_and_sizing() {
        let bad_version = r#"{
            "protocol_version":2,
            "vmm_version":"0.1.0",
            "cpu_template":"bare",
            "ram_mib":256,
            "vcpu_count":1,
            "device_model_hash":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
            "volume_ids":[]
        }"#;
        assert!(serde_json::from_str::<NegotiationRequest>(bad_version).is_err());

        let bad_ram = NegotiationRequest {
            protocol_version: SUPPORTED_PROTOCOL_VERSION,
            vmm_version: "0.1.0".into(),
            cpu_template: "bare".into(),
            ram_mib: 0,
            vcpu_count: 1,
            device_model_hash: [0; 32],
            volume_ids: vec![],
        };
        assert!(matches!(
            bad_ram.validate(),
            Err(NegotiationError::InvalidRamMiB(0))
        ));
    }

    #[test]
    fn request_rejects_too_many_volume_ids() {
        let mut r = NegotiationRequest {
            protocol_version: SUPPORTED_PROTOCOL_VERSION,
            vmm_version: "0.1.0".into(),
            cpu_template: "bare".into(),
            ram_mib: 256,
            vcpu_count: 1,
            device_model_hash: [0; 32],
            volume_ids: vec!["vol".into(); MAX_VOLUME_IDS + 1],
        };
        assert!(matches!(
            r.validate(),
            Err(NegotiationError::TooManyVolumeIds { .. })
        ));

        r.volume_ids = vec!["x".repeat(MAX_VOLUME_ID_BYTES + 1)];
        assert!(matches!(
            r.validate(),
            Err(NegotiationError::StringTooLong {
                field: "volume_id",
                ..
            })
        ));
    }

    #[test]
    fn response_reject_reason_is_bounded() {
        let json = format!(
            r#"{{"Reject":"{}"}}"#,
            "x".repeat(MAX_REJECT_REASON_BYTES + 1)
        );
        assert!(serde_json::from_str::<NegotiationResponse>(&json).is_err());
    }
}
