//! Declarative VM configuration (kernel path, memory size, vcpu count, volumes, net).

use serde::{Deserialize, Serialize};

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
        Ok(())
    }
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
}
