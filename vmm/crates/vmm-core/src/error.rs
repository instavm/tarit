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
    #[error("snapshot error: {0}")]
    Snapshot(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, VmmError>;

impl From<tarit_proto::config::ConfigError> for VmmError {
    fn from(e: tarit_proto::config::ConfigError) -> Self {
        VmmError::InvalidConfig(e.to_string())
    }
}
