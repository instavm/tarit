use thiserror::Error;

#[derive(Debug, Error)]
pub enum VmmError {
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("kvm error: {0}")]
    Kvm(String),
    #[error("memory error: {0}")]
    Memory(String),
    #[error("loader error: {0}")]
    Loader(String),
    #[error("device error: {0}")]
    Device(String),
    #[error("device error: {message}")]
    IoQuiescence {
        message: String,
        /// Whether every partially parked worker was confirmed running again.
        /// A false value requires callers to keep vCPUs paused.
        vcpus_may_resume: bool,
    },
    #[error("snapshot error: {0}")]
    Snapshot(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, VmmError>;

impl VmmError {
    #[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "kvm"))]
    pub(crate) fn vcpus_may_resume_after_io_error(&self) -> bool {
        match self {
            Self::IoQuiescence {
                vcpus_may_resume, ..
            } => *vcpus_may_resume,
            _ => true,
        }
    }
}

impl From<tarit_proto::config::ConfigError> for VmmError {
    fn from(e: tarit_proto::config::ConfigError) -> Self {
        VmmError::InvalidConfig(e.to_string())
    }
}
