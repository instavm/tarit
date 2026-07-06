mod api;
mod args;
mod report;
mod stats;

use std::{sync::Arc, time::Instant};

use anyhow::Result;
use args::{Args, Mode};
use clap::Parser;
use report::{write_report, IterationResult, ModeOutcome, RampPoint, RunContext};

type SharedClient = Arc<api::TaritClient>;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let client = Arc::new(api::TaritClient::new(&args.url, &args.api_key)?);
    let ctx = RunContext::from_args(&args);

    if args.warmup > 0 {
        run_warmups(client.clone(), &ctx, args.warmup).await;
    }

    for mode in args.mode.modes_to_run() {
        let outcome = match mode {
            Mode::Sequential => run_sequential(client.clone(), &ctx).await,
            Mode::Staggered => run_staggered(client.clone(), &ctx).await,
            Mode::Burst => run_burst(client.clone(), &ctx).await,
            Mode::All => unreachable!("all is expanded before execution"),
        };
        let path = write_report(&ctx, outcome)?;
        println!("wrote {}", path.display());
    }

    Ok(())
}

async fn run_warmups(client: SharedClient, ctx: &RunContext, warmups: usize) {
    eprintln!("running {warmups} warmup iteration(s)");
    for _ in 0..warmups {
        let _ = run_iteration(client.clone(), ctx).await;
    }
}

async fn run_sequential(client: SharedClient, ctx: &RunContext) -> ModeOutcome {
    let mut iterations = Vec::with_capacity(ctx.iterations);

    for _ in 0..ctx.iterations {
        iterations.push(run_iteration(client.clone(), ctx).await);
    }

    ModeOutcome::new(Mode::Sequential, iterations).print_summary(&ctx.provider)
}

async fn run_burst(client: SharedClient, ctx: &RunContext) -> ModeOutcome {
    let started = Instant::now();
    let mut handles = Vec::with_capacity(ctx.concurrency);

    for index in 0..ctx.concurrency {
        let client = client.clone();
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move {
            (index, run_iteration(client, &ctx).await)
        }));
    }

    let mut ordered = vec![None; ctx.concurrency];
    for handle in handles {
        match handle.await {
            Ok((index, result)) => ordered[index] = Some(result),
            Err(err) => eprintln!("burst task join error: {err}"),
        }
    }

    let iterations = fill_join_failures(ordered);
    ModeOutcome::new(Mode::Burst, iterations)
        .with_concurrency(ctx.concurrency)
        .with_wall_clock(started.elapsed())
        .print_summary(&ctx.provider)
}

async fn run_staggered(client: SharedClient, ctx: &RunContext) -> ModeOutcome {
    let started = Instant::now();
    let mut handles = Vec::with_capacity(ctx.concurrency);

    for index in 0..ctx.concurrency {
        if index > 0 {
            tokio::time::sleep(ctx.stagger_delay).await;
        }
        let offset = started.elapsed();
        let client = client.clone();
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move {
            let result = run_iteration(client, &ctx).await;
            (index, RampPoint::new(offset, result.tti_ms), result)
        }));
    }

    let mut ordered = vec![None; ctx.concurrency];
    let mut ramp = vec![None; ctx.concurrency];
    for handle in handles {
        match handle.await {
            Ok((index, ramp_point, result)) => {
                ordered[index] = Some(result);
                ramp[index] = Some(ramp_point);
            }
            Err(err) => eprintln!("staggered task join error: {err}"),
        }
    }

    let iterations = fill_join_failures(ordered);
    let ramp_profile = ramp
        .into_iter()
        .enumerate()
        .map(|(index, point)| {
            point.unwrap_or_else(|| {
                RampPoint::from_millis((index as u64) * ctx.stagger_delay.as_millis() as u64, 0)
            })
        })
        .collect();

    ModeOutcome::new(Mode::Staggered, iterations)
        .with_concurrency(ctx.concurrency)
        .with_stagger_delay(ctx.stagger_delay)
        .with_wall_clock(started.elapsed())
        .with_ramp_profile(ramp_profile)
        .print_summary(&ctx.provider)
}

async fn run_iteration(client: SharedClient, ctx: &RunContext) -> IterationResult {
    match iteration_inner(client, ctx).await {
        Ok(tti_ms) => IterationResult::success(tti_ms),
        Err(err) => IterationResult::failure(err.to_string()),
    }
}

async fn iteration_inner(client: SharedClient, ctx: &RunContext) -> Result<u64> {
    let started = Instant::now();
    let deadline = started + ctx.timeout;
    let mut vm_id = None;

    let result = async {
        let vm = tokio::time::timeout(remaining(deadline), client.create_vm(ctx)).await??;
        if vm.status != "running" {
            anyhow::bail!("created VM {} returned status {}", vm.id, vm.status);
        }
        vm_id = Some(vm.id);

        let exec_timeout_ms = ctx.timeout_ms.min(30_000);
        let execution = tokio::time::timeout(
            remaining(deadline),
            client.execute(vm.id, &ctx.command, exec_timeout_ms),
        )
        .await??;
        match execution.status.as_str() {
            "completed" if execution.exit_code == Some(0) => {
                Ok(started.elapsed().as_millis() as u64)
            }
            "completed" => anyhow::bail!(
                "execution {} completed with exit code {:?}",
                execution.id,
                execution.exit_code
            ),
            "failed" => {
                let detail = execution
                    .error
                    .unwrap_or_else(|| "execution failed".to_string());
                anyhow::bail!("execution {} failed: {detail}", execution.id)
            }
            other => anyhow::bail!("execution {} returned unknown status {other}", execution.id),
        }
    }
    .await;

    if let Some(id) = vm_id {
        let _ = client.delete_vm(id).await;
    }

    match result {
        Ok(tti_ms) => Ok(tti_ms),
        Err(err) if err.is::<tokio::time::error::Elapsed>() => anyhow::bail!("iteration timed out"),
        Err(err) => Err(err),
    }
}

fn remaining(deadline: Instant) -> std::time::Duration {
    deadline
        .checked_duration_since(Instant::now())
        .unwrap_or_default()
}

fn fill_join_failures(results: Vec<Option<IterationResult>>) -> Vec<IterationResult> {
    results
        .into_iter()
        .map(|result| {
            result.unwrap_or_else(|| IterationResult::failure("task join failed".to_string()))
        })
        .collect()
}
