use std::{fs, path::PathBuf, time::Duration};

use anyhow::Result;
use chrono::{SecondsFormat, Utc};
use serde::Serialize;

use crate::{args, stats};

#[derive(Debug, Clone)]
pub struct RunContext {
    pub iterations: usize,
    pub concurrency: usize,
    pub stagger_delay: Duration,
    pub timeout: Duration,
    pub timeout_ms: u64,
    #[allow(dead_code)]
    pub poll_interval: Duration,
    pub command: String,
    pub rootfs: Option<String>,
    pub kernel_path: Option<String>,
    pub memory_mib: u64,
    pub vcpus: u8,
    pub provider: String,
    pub results_dir: PathBuf,
}

impl RunContext {
    pub fn from_args(args: &args::Args) -> Self {
        Self {
            iterations: args.iterations,
            concurrency: args.concurrency,
            stagger_delay: Duration::from_millis(args.stagger_delay_ms),
            timeout: Duration::from_millis(args.timeout_ms),
            timeout_ms: args.timeout_ms,
            poll_interval: Duration::from_millis(15),
            command: args.command.clone(),
            rootfs: args.rootfs.clone(),
            kernel_path: args.kernel_path.clone(),
            memory_mib: args.memory_mib,
            vcpus: args.vcpus,
            provider: args.provider.clone(),
            results_dir: args.results_dir.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IterationResult {
    pub tti_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl IterationResult {
    pub fn success(tti_ms: u64) -> Self {
        Self {
            tti_ms,
            error: None,
        }
    }

    pub fn failure(error: String) -> Self {
        Self {
            tti_ms: 0,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RampPoint {
    pub offset_ms: u64,
    pub tti_ms: u64,
}

impl RampPoint {
    pub fn new(offset: Duration, tti_ms: u64) -> Self {
        Self {
            offset_ms: offset.as_millis() as u64,
            tti_ms,
        }
    }

    pub fn from_millis(offset_ms: u64, tti_ms: u64) -> Self {
        Self { offset_ms, tti_ms }
    }
}

#[derive(Debug, Clone)]
pub struct ModeOutcome {
    mode: args::Mode,
    iterations: Vec<IterationResult>,
    concurrency: Option<usize>,
    stagger_delay: Option<Duration>,
    wall_clock: Option<Duration>,
    ramp_profile: Option<Vec<RampPoint>>,
}

impl ModeOutcome {
    pub fn new(mode: args::Mode, iterations: Vec<IterationResult>) -> Self {
        Self {
            mode,
            iterations,
            concurrency: None,
            stagger_delay: None,
            wall_clock: None,
            ramp_profile: None,
        }
    }

    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = Some(concurrency);
        self
    }

    pub fn with_stagger_delay(mut self, stagger_delay: Duration) -> Self {
        self.stagger_delay = Some(stagger_delay);
        self
    }

    pub fn with_wall_clock(mut self, wall_clock: Duration) -> Self {
        self.wall_clock = Some(wall_clock);
        self
    }

    pub fn with_ramp_profile(mut self, ramp_profile: Vec<RampPoint>) -> Self {
        self.ramp_profile = Some(ramp_profile);
        self
    }

    pub fn print_summary(self, provider: &str) -> Self {
        let result = self.to_result(provider);
        let tti = &result.summary.tti_ms;
        let success_pct = result.success_rate * 100.0;
        println!(
            "provider={} mode={} n={} success={:.1}% median={}ms p95={}ms p99={}ms score={:.2}",
            provider,
            self.mode.as_str(),
            self.iterations.len(),
            success_pct,
            display_stat(tti.as_ref().map(|stats| stats.median)),
            display_stat(tti.as_ref().map(|stats| stats.p95)),
            display_stat(tti.as_ref().map(|stats| stats.p99)),
            result.composite_score
        );
        self
    }

    fn to_result(&self, provider: &str) -> BenchmarkResult {
        let successful = self
            .iterations
            .iter()
            .filter(|iteration| iteration.error.is_none())
            .map(|iteration| iteration.tti_ms)
            .collect::<Vec<_>>();
        let success_rate = if self.iterations.is_empty() {
            0.0
        } else {
            successful.len() as f64 / self.iterations.len() as f64
        };
        let stats = stats::summarize(&successful);
        let time_to_first_ready_ms = match self.mode {
            args::Mode::Staggered | args::Mode::Burst => successful.iter().min().copied(),
            _ => None,
        };

        BenchmarkResult {
            provider: provider.to_string(),
            mode: self.mode.as_str().to_string(),
            iterations: self.iterations.clone(),
            summary: Summary { tti_ms: stats },
            composite_score: round2(stats::composite_score(stats, success_rate)),
            success_rate,
            concurrency: self.concurrency,
            stagger_delay_ms: self
                .stagger_delay
                .map(|duration| duration.as_millis() as u64),
            wall_clock_ms: self.wall_clock.map(|duration| duration.as_millis() as u64),
            time_to_first_ready_ms,
            ramp_profile: self.ramp_profile.clone(),
        }
    }

    fn mode_dir(&self) -> String {
        format!("{}_tti", self.mode.as_str())
    }
}

pub fn write_report(ctx: &RunContext, outcome: ModeOutcome) -> Result<PathBuf> {
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let report = Report {
        version: "1.1",
        timestamp: timestamp.clone(),
        environment: Environment::current(),
        config: Config::for_mode(ctx, outcome.mode),
        results: vec![outcome.to_result(&ctx.provider)],
    };

    let dir = ctx.results_dir.join(outcome.mode_dir());
    fs::create_dir_all(&dir)?;
    let file_name = format!("{}.json", timestamp.replace(':', "-"));
    let path = dir.join(file_name);
    let bytes = serde_json::to_vec_pretty(&report)?;
    fs::write(&path, bytes)?;
    fs::copy(&path, dir.join("latest.json"))?;
    Ok(path)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    version: &'static str,
    timestamp: String,
    environment: Environment,
    config: Config,
    results: Vec<BenchmarkResult>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Environment {
    node: &'static str,
    platform: &'static str,
    arch: &'static str,
}

impl Environment {
    fn current() -> Self {
        Self {
            node: "n/a",
            platform: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    iterations: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    concurrency: Option<usize>,
    timeout_ms: u64,
}

impl Config {
    fn for_mode(ctx: &RunContext, mode: args::Mode) -> Self {
        match mode {
            args::Mode::Sequential => Self {
                iterations: Some(ctx.iterations),
                concurrency: None,
                timeout_ms: ctx.timeout_ms,
            },
            args::Mode::Staggered | args::Mode::Burst => Self {
                iterations: None,
                concurrency: Some(ctx.concurrency),
                timeout_ms: ctx.timeout_ms,
            },
            args::Mode::All => unreachable!("all is not written as a benchmark mode"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchmarkResult {
    provider: String,
    mode: String,
    iterations: Vec<IterationResult>,
    summary: Summary,
    composite_score: f64,
    success_rate: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    concurrency: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stagger_delay_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wall_clock_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    time_to_first_ready_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ramp_profile: Option<Vec<RampPoint>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    tti_ms: Option<stats::TtiStats>,
}

fn display_stat(value: Option<u64>) -> String {
    value.map_or_else(|| "n/a".to_string(), |value| value.to_string())
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}
