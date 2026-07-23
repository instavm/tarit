//! Declarative VM configuration (kernel path, memory size, vcpu count, volumes, net).

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// One mebibyte in bytes.
pub const MIB: u64 = 1024 * 1024;
/// Minimum guest RAM accepted by config validation.
pub const MIN_MEMORY_MIB: u64 = 1;
/// Maximum guest RAM accepted by config validation (1 TiB).
pub const MAX_MEMORY_MIB: u64 = 1024 * 1024;
/// Maximum guest RAM accepted by config validation, in bytes.
pub const MAX_MEMORY_BYTES: u64 = MAX_MEMORY_MIB * MIB;
/// Maximum number of vCPUs accepted by config validation.
pub const MAX_VCPU_COUNT: u16 = 256;
/// Maximum number of network devices supported by the deterministic MMIO map.
pub const MAX_NET_DEVICES: usize = 8;

/// Error returned when a VM config fails resource-sizing validation.
#[derive(Debug)]
pub enum ConfigError {
    /// A field is outside its accepted range or would overflow.
    Invalid(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Invalid(msg) => write!(f, "invalid configuration: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Top-level VM configuration consumed by the VMM at create time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmConfig {
    pub kernel: KernelConfig,
    pub memory: MemoryConfig,
    pub vcpus: VcpuConfig,
    #[serde(default)]
    pub volumes: Vec<VolumeConfig>,
    #[serde(default)]
    pub net: Vec<NetConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelConfig {
    /// Path to an uncompressed vmlinux ELF or a bzImage.
    pub path: String,
    /// Kernel command line.
    pub cmdline: String,
    /// Optional initramfs path.
    pub initramfs: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    /// Guest RAM in MiB.
    pub size_mib: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VcpuConfig {
    pub count: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeConfig {
    pub path: String,
    /// true = read-only (rootfs), false = read-write (data volume).
    pub read_only: bool,
    /// Private sparse CoW overlay path. When set, `path` is the read-only base.
    #[serde(default)]
    pub overlay: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetConfig {
    /// Tap interface name on the host (pre-created by the orchestrator).
    pub tap: String,
    /// Guest MAC (optional; auto-assigned if not set).
    #[serde(default)]
    pub guest_mac: Option<String>,
    /// Guest IP (for port forwarding + egress rules).
    #[serde(default)]
    pub guest_ip: Option<String>,
    /// Host ports to forward to the guest.
    #[serde(default)]
    pub port_forwards: Vec<PortForwardConfig>,
}

/// A port-forwarding rule: host_port → guest_ip:guest_port.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortForwardConfig {
    pub host_port: u16,
    pub guest_port: u16,
    #[serde(default = "default_proto")]
    pub proto: String,
}

fn default_proto() -> String {
    "tcp".into()
}

impl VmConfig {
    /// Validate resource sizing fields before they are used for allocations.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.memory.validate()?;
        self.vcpus.validate()?;
        validate_networks(&self.net)?;
        Ok(())
    }
}

fn validate_networks(networks: &[NetConfig]) -> Result<(), ConfigError> {
    if networks.len() > MAX_NET_DEVICES {
        return Err(ConfigError::Invalid(format!(
            "net supports at most {MAX_NET_DEVICES} devices, got {}",
            networks.len()
        )));
    }

    let mut taps = HashSet::new();
    let mut macs = HashSet::new();
    let mut ips = HashSet::new();
    let mut host_ports = HashSet::new();
    for (index, net) in networks.iter().enumerate() {
        if net.tap.is_empty()
            || net.tap.len() >= libc_if_name_size()
            || !net
                .tap
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(ConfigError::Invalid(format!(
                "net[{index}].tap must be 1..={} ASCII [A-Za-z0-9_.-] bytes",
                libc_if_name_size() - 1
            )));
        }
        if !taps.insert(net.tap.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "duplicate tap interface {:?}",
                net.tap
            )));
        }

        if let Some(mac) = &net.guest_mac {
            let parsed = parse_mac(mac).ok_or_else(|| {
                ConfigError::Invalid(format!(
                    "net[{index}].guest_mac must be six colon-separated hex bytes"
                ))
            })?;
            if parsed == [0; 6] || parsed[0] & 1 != 0 {
                return Err(ConfigError::Invalid(format!(
                    "net[{index}].guest_mac must be a non-zero unicast address"
                )));
            }
            if !macs.insert(parsed) {
                return Err(ConfigError::Invalid(format!("duplicate guest MAC {mac:?}")));
            }
        }

        if let Some(ip) = &net.guest_ip {
            let parsed = ip.parse::<std::net::IpAddr>().map_err(|error| {
                ConfigError::Invalid(format!("net[{index}].guest_ip {ip:?}: {error}"))
            })?;
            if parsed.is_unspecified() || parsed.is_multicast() {
                return Err(ConfigError::Invalid(format!(
                    "net[{index}].guest_ip must be a unicast address"
                )));
            }
            if !ips.insert(parsed) {
                return Err(ConfigError::Invalid(format!("duplicate guest IP {ip:?}")));
            }
        } else if !net.port_forwards.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "net[{index}] needs guest_ip when port_forwards are configured"
            )));
        }

        for (forward_index, forward) in net.port_forwards.iter().enumerate() {
            if forward.host_port == 0 || forward.guest_port == 0 {
                return Err(ConfigError::Invalid(format!(
                    "net[{index}].port_forwards[{forward_index}] ports must be non-zero"
                )));
            }
            if !matches!(forward.proto.as_str(), "tcp" | "udp") {
                return Err(ConfigError::Invalid(format!(
                    "net[{index}].port_forwards[{forward_index}].proto must be tcp or udp"
                )));
            }
            if !host_ports.insert((forward.proto.as_str(), forward.host_port)) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate host port {}/{}",
                    forward.host_port, forward.proto
                )));
            }
        }
    }
    Ok(())
}

const fn libc_if_name_size() -> usize {
    // Linux IFNAMSIZ. This protocol is consumed by the Linux VMM; keeping the
    // bound here also makes invalid names fail before any host networking call.
    16
}

fn parse_mac(value: &str) -> Option<[u8; 6]> {
    let mut result = [0u8; 6];
    let mut parts = value.split(':');
    for byte in &mut result {
        let part = parts.next()?;
        if part.len() != 2 {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    parts.next().is_none().then_some(result)
}

impl MemoryConfig {
    /// Return guest RAM in bytes after overflow and range validation.
    #[must_use = "handle the validated byte size or validation error"]
    pub fn size_bytes(&self) -> Result<u64, ConfigError> {
        let bytes = self
            .size_mib
            .checked_mul(MIB)
            .ok_or_else(|| ConfigError::Invalid("memory size overflows bytes".into()))?;
        if !(MIN_MEMORY_MIB..=MAX_MEMORY_MIB).contains(&self.size_mib) {
            return Err(ConfigError::Invalid(format!(
                "memory.size_mib must be in {MIN_MEMORY_MIB}..={MAX_MEMORY_MIB}, got {}",
                self.size_mib
            )));
        }
        Ok(bytes)
    }

    /// Validate guest RAM sizing without returning the converted byte count.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.size_bytes().map(|_| ())
    }
}

impl VcpuConfig {
    /// Validate guest vCPU sizing.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.count == 0 {
            return Err(ConfigError::Invalid(
                "vcpus.count must be at least 1".into(),
            ));
        }
        if u16::from(self.count) > MAX_VCPU_COUNT {
            return Err(ConfigError::Invalid(format!(
                "vcpus.count must be <= {MAX_VCPU_COUNT}, got {}",
                self.count
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_rejects_unknown_fields() {
        // Hardening: an unexpected/injected field is rejected
        // (deny_unknown_fields) rather than silently ignored.
        let top = r#"{"kernel":{"path":"/k","cmdline":"","initramfs":null},"memory":{"size_mib":64},"vcpus":{"count":1},"evil":true}"#;
        assert!(serde_json::from_str::<VmConfig>(top).is_err());
        let nested = r#"{"kernel":{"path":"/k","cmdline":"","initramfs":null,"x":1},"memory":{"size_mib":64},"vcpus":{"count":1}}"#;
        assert!(serde_json::from_str::<VmConfig>(nested).is_err());
    }

    fn config_with_net(net: Vec<NetConfig>) -> VmConfig {
        VmConfig {
            kernel: KernelConfig {
                path: "/kernel".into(),
                cmdline: String::new(),
                initramfs: None,
            },
            memory: MemoryConfig { size_mib: 64 },
            vcpus: VcpuConfig { count: 1 },
            volumes: Vec::new(),
            net,
        }
    }

    fn net(tap: &str, mac: &str, ip: &str) -> NetConfig {
        NetConfig {
            tap: tap.into(),
            guest_mac: Some(mac.into()),
            guest_ip: Some(ip.into()),
            port_forwards: Vec::new(),
        }
    }

    #[test]
    fn config_validates_network_identity_and_uniqueness() {
        config_with_net(vec![
            net("tap-a", "02:00:00:00:00:01", "10.0.0.2"),
            net("tap-b", "02:00:00:00:00:02", "10.0.0.3"),
        ])
        .validate()
        .unwrap();

        assert!(
            config_with_net(vec![net("bad/tap", "02:00:00:00:00:01", "10.0.0.2")])
                .validate()
                .is_err()
        );
        assert!(
            config_with_net(vec![net("tap-a", "03:00:00:00:00:01", "10.0.0.2")])
                .validate()
                .is_err()
        );
        assert!(config_with_net(vec![
            net("tap-a", "02:00:00:00:00:01", "10.0.0.2"),
            net("tap-a", "02:00:00:00:00:02", "10.0.0.3"),
        ])
        .validate()
        .is_err());
        assert!(config_with_net(vec![
            net("tap-a", "02:00:00:00:00:01", "10.0.0.2"),
            net("tap-b", "02:00:00:00:00:01", "10.0.0.3"),
        ])
        .validate()
        .is_err());
    }

    #[test]
    fn config_validates_port_forward_identity() {
        let mut first = net("tap-a", "02:00:00:00:00:01", "10.0.0.2");
        first.port_forwards.push(PortForwardConfig {
            host_port: 8080,
            guest_port: 80,
            proto: "tcp".into(),
        });
        let mut second = net("tap-b", "02:00:00:00:00:02", "10.0.0.3");
        second.port_forwards.push(PortForwardConfig {
            host_port: 8080,
            guest_port: 8080,
            proto: "tcp".into(),
        });
        assert!(config_with_net(vec![first, second]).validate().is_err());
    }
}
