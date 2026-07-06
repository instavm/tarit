//! Migration transport — authenticated (mTLS) page/state channel with
//! optional zstd compression and multi-stream parallelism for large RAM.
//! Scaffold (M15).

use serde::{Deserialize, Deserializer, Serialize};
use std::net::SocketAddr;
use std::str::FromStr;

pub const MAX_ADDR_BYTES: usize = 255;
pub const MAX_PATH_BYTES: usize = 4096;
pub const MIN_PARALLEL_STREAMS: u8 = 1;
pub const MAX_PARALLEL_STREAMS: u8 = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportConfig {
    #[serde(deserialize_with = "deserialize_listen_addr")]
    pub listen_addr: String,
    #[serde(deserialize_with = "deserialize_path")]
    pub mtls_cert: String,
    #[serde(deserialize_with = "deserialize_path")]
    pub mtls_key: String,
    #[serde(deserialize_with = "deserialize_path")]
    pub mtls_ca: String,
    pub compression: Compression,
    #[serde(deserialize_with = "deserialize_parallel_streams")]
    pub parallel_streams: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Compression {
    None,
    Zstd,
}

impl TransportConfig {
    pub fn validate(&self) -> Result<(), TransportError> {
        validate_listen_addr(&self.listen_addr)?;
        validate_path("mtls_cert", &self.mtls_cert)?;
        validate_path("mtls_key", &self.mtls_key)?;
        validate_path("mtls_ca", &self.mtls_ca)?;
        validate_parallel_streams(self.parallel_streams)?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TransportError {
    #[error("listen_addr too long: {len} > {max}")]
    AddressTooLong { len: usize, max: usize },
    #[error("invalid listen_addr: {0}")]
    InvalidAddress(String),
    #[error("{field} path is empty")]
    EmptyPath { field: &'static str },
    #[error("{field} path too long: {len} > {max}")]
    PathTooLong {
        field: &'static str,
        len: usize,
        max: usize,
    },
    #[error("{field} path contains NUL byte")]
    PathContainsNul { field: &'static str },
    #[error("parallel_streams out of range: {0}")]
    InvalidParallelStreams(u8),
}

fn validate_listen_addr(addr: &str) -> Result<(), TransportError> {
    if addr.len() > MAX_ADDR_BYTES {
        return Err(TransportError::AddressTooLong {
            len: addr.len(),
            max: MAX_ADDR_BYTES,
        });
    }
    SocketAddr::from_str(addr)
        .map(|_| ())
        .map_err(|e| TransportError::InvalidAddress(e.to_string()))
}

fn validate_path(field: &'static str, path: &str) -> Result<(), TransportError> {
    if path.is_empty() {
        return Err(TransportError::EmptyPath { field });
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(TransportError::PathTooLong {
            field,
            len: path.len(),
            max: MAX_PATH_BYTES,
        });
    }
    if path.as_bytes().contains(&0) {
        return Err(TransportError::PathContainsNul { field });
    }
    Ok(())
}

fn validate_parallel_streams(streams: u8) -> Result<(), TransportError> {
    if !(MIN_PARALLEL_STREAMS..=MAX_PARALLEL_STREAMS).contains(&streams) {
        return Err(TransportError::InvalidParallelStreams(streams));
    }
    Ok(())
}

fn deserialize_listen_addr<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let addr = String::deserialize(deserializer)?;
    validate_listen_addr(&addr).map_err(serde::de::Error::custom)?;
    Ok(addr)
}

fn deserialize_path<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let path = String::deserialize(deserializer)?;
    validate_path("mtls path", &path).map_err(serde::de::Error::custom)?;
    Ok(path)
}

fn deserialize_parallel_streams<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: Deserializer<'de>,
{
    let streams = u8::deserialize(deserializer)?;
    validate_parallel_streams(streams).map_err(serde::de::Error::custom)?;
    Ok(streams)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_config_round_trips() {
        let t = TransportConfig {
            listen_addr: "10.0.0.1:4444".into(),
            mtls_cert: "/cert.pem".into(),
            mtls_key: "/key.pem".into(),
            mtls_ca: "/ca.pem".into(),
            compression: Compression::Zstd,
            parallel_streams: 4,
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: TransportConfig = serde_json::from_str(&s).unwrap();
        back.validate().unwrap();
        assert_eq!(back.listen_addr, "10.0.0.1:4444");
        assert_eq!(back.compression, Compression::Zstd);
        assert_eq!(back.parallel_streams, 4);
    }

    #[test]
    fn compression_default_is_none() {
        // The PRD §9d "optional zstd compression" — None is the default.
        let c = Compression::None;
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains("None"));
    }

    #[test]
    fn transport_rejects_invalid_address_and_streams() {
        let mut t = TransportConfig {
            listen_addr: "not-an-address".into(),
            mtls_cert: "/cert.pem".into(),
            mtls_key: "/key.pem".into(),
            mtls_ca: "/ca.pem".into(),
            compression: Compression::None,
            parallel_streams: 1,
        };
        assert!(matches!(
            t.validate(),
            Err(TransportError::InvalidAddress(_))
        ));

        t.listen_addr = "127.0.0.1:4444".into();
        t.parallel_streams = 0;
        assert!(matches!(
            t.validate(),
            Err(TransportError::InvalidParallelStreams(0))
        ));
    }

    #[test]
    fn transport_rejects_bad_paths() {
        let t = TransportConfig {
            listen_addr: "127.0.0.1:4444".into(),
            mtls_cert: "".into(),
            mtls_key: "/key.pem".into(),
            mtls_ca: "/ca.pem".into(),
            compression: Compression::None,
            parallel_streams: 1,
        };
        assert!(matches!(
            t.validate(),
            Err(TransportError::EmptyPath { field: "mtls_cert" })
        ));

        let json = r#"{
            "listen_addr":"127.0.0.1:4444",
            "mtls_cert":"",
            "mtls_key":"/key.pem",
            "mtls_ca":"/ca.pem",
            "compression":"None",
            "parallel_streams":1
        }"#;
        assert!(serde_json::from_str::<TransportConfig>(json).is_err());
    }
}
