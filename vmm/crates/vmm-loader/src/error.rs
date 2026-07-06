use thiserror::Error;

#[derive(Debug, Error)]
pub enum LoaderError {
    #[error("kernel image invalid: {0}")]
    InvalidKernel(String),
    #[error("kernel load failed: {0}")]
    Load(String),
    #[error("cmdline too long: {len} > {max}")]
    CmdlineTooLong { len: usize, max: usize },
    #[error("boot configurator: {0}")]
    BootConfig(String),
    #[error("initramfs: {0}")]
    Initramfs(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, LoaderError>;
