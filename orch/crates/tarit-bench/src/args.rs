use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "tarit-bench",
    about = "Measure taritd Time-To-Interactive benchmarks"
)]
pub struct Args {
    #[arg(value_enum, default_value_t = Mode::All)]
    pub mode: Mode,

    #[arg(long, env = "TARIT_URL", default_value = "http://127.0.0.1:8080")]
    pub url: String,

    #[arg(long, env = "TARIT_API_KEY", default_value = "test-key")]
    pub api_key: String,

    #[arg(long, default_value_t = 100)]
    pub iterations: usize,

    #[arg(long, default_value_t = 100)]
    pub concurrency: usize,

    #[arg(long, default_value_t = 200)]
    pub stagger_delay_ms: u64,

    #[arg(long, default_value_t = 120_000)]
    pub timeout_ms: u64,

    #[arg(long, default_value = "node -v")]
    pub command: String,

    #[arg(long)]
    pub rootfs: Option<String>,

    #[arg(long)]
    pub kernel_path: Option<String>,

    #[arg(long, default_value_t = 256)]
    pub memory_mib: u64,

    #[arg(long, default_value_t = 1)]
    pub vcpus: u8,

    #[arg(long, default_value = "instavm")]
    pub provider: String,

    #[arg(long, default_value = "./bench-results")]
    pub results_dir: PathBuf,

    #[arg(long, default_value_t = 0)]
    pub warmup: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    Sequential,
    Staggered,
    Burst,
    All,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Staggered => "staggered",
            Self::Burst => "burst",
            Self::All => "all",
        }
    }

    pub fn modes_to_run(self) -> Vec<Self> {
        match self {
            Self::All => vec![Self::Sequential, Self::Staggered, Self::Burst],
            mode => vec![mode],
        }
    }
}
