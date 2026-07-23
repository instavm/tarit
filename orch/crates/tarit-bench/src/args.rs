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

    /// Startup path this run must actually exercise. Cold/warm are verified
    /// against the server-reported lifecycle branch; snapshot/suspend require
    /// dedicated workflows and are rejected by the create-only runner.
    #[arg(long, value_enum, default_value_t = StartupPath::Unspecified)]
    pub startup_path: StartupPath,

    /// Fail the run when median Time-To-Interactive exceeds this value.
    #[arg(long)]
    pub max_median_ms: Option<u64>,

    /// Fail the run when p95 Time-To-Interactive exceeds this value.
    #[arg(long)]
    pub max_p95_ms: Option<u64>,

    /// Fail the run when p99 Time-To-Interactive exceeds this value.
    #[arg(long)]
    pub max_p99_ms: Option<u64>,

    /// Fail the run when the successful-iteration percentage is lower.
    #[arg(long)]
    pub min_success_percent: Option<f64>,
}

impl Args {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.iterations == 0 {
            anyhow::bail!("--iterations must be greater than zero");
        }
        if self.concurrency == 0 {
            anyhow::bail!("--concurrency must be greater than zero");
        }
        if self.timeout_ms == 0 {
            anyhow::bail!("--timeout-ms must be greater than zero");
        }
        let has_gate = self.max_median_ms.is_some()
            || self.max_p95_ms.is_some()
            || self.max_p99_ms.is_some()
            || self.min_success_percent.is_some();
        if has_gate && self.startup_path == StartupPath::Unspecified {
            anyhow::bail!("performance gates require an explicit --startup-path");
        }
        if matches!(
            self.startup_path,
            StartupPath::Snapshot | StartupPath::Suspend
        ) {
            anyhow::bail!(
                "--startup-path {} is not supported by the create-only benchmark workflow",
                self.startup_path.as_str()
            );
        }
        if self
            .min_success_percent
            .is_some_and(|value| !(0.0..=100.0).contains(&value))
        {
            anyhow::bail!("--min-success-percent must be between 0 and 100");
        }
        let sample_counts: &[usize] = match self.mode {
            Mode::Sequential => std::slice::from_ref(&self.iterations),
            Mode::Staggered | Mode::Burst => std::slice::from_ref(&self.concurrency),
            Mode::All => &[self.iterations, self.concurrency],
        };
        if self.max_p95_ms.is_some() && sample_counts.iter().any(|count| *count < 20) {
            anyhow::bail!("a p95 gate requires at least 20 samples per selected mode");
        }
        if self.max_p99_ms.is_some() && sample_counts.iter().any(|count| *count < 100) {
            anyhow::bail!("a p99 gate requires at least 100 samples per selected mode");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StartupPath {
    Cold,
    Snapshot,
    Suspend,
    Warm,
    Unspecified,
}

impl StartupPath {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Snapshot => "snapshot",
            Self::Suspend => "suspend",
            Self::Warm => "warm",
            Self::Unspecified => "unspecified",
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gates_require_launch_provenance() {
        let args =
            Args::try_parse_from(["tarit-bench", "sequential", "--max-median-ms", "100"]).unwrap();
        assert!(args
            .validate()
            .unwrap_err()
            .to_string()
            .contains("--startup-path"));
    }

    #[test]
    fn tail_gates_require_enough_samples() {
        let args = Args::try_parse_from([
            "tarit-bench",
            "sequential",
            "--iterations",
            "99",
            "--startup-path",
            "cold",
            "--max-p99-ms",
            "1000",
        ])
        .unwrap();
        assert!(args
            .validate()
            .unwrap_err()
            .to_string()
            .contains("at least 100"));
    }

    #[test]
    fn accepts_proven_hundred_sample_p99_gate() {
        let args = Args::try_parse_from([
            "tarit-bench",
            "sequential",
            "--iterations",
            "100",
            "--startup-path",
            "warm",
            "--max-p99-ms",
            "1000",
            "--min-success-percent",
            "100",
        ])
        .unwrap();
        assert!(args.validate().is_ok());
    }
}
