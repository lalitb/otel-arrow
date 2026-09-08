// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded standalone capture with human or machine-readable summary output.

#![allow(clippy::print_stdout)]

use std::{
    num::{NonZeroU32, NonZeroUsize},
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

use clap::{Parser, ValueEnum};
use otel_arrow_dfe_ebpf_profiler::{
    CpuSelection, MemoryEstimate, ProfileSnapshot, ProfilerBuilder, ProfilerConfig, ProfilerError,
    ProgramConfig, ShardingStrategy, SymbolizationConfig,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Output {
    Human,
    Json,
}

#[derive(Debug, Parser)]
#[command(about = "Capture bounded native CPU profile summaries; never print host stacks")]
struct Arguments {
    #[arg(long, default_value = "10s", value_parser = bounded_duration)]
    duration: Duration,
    #[arg(long, default_value = "2s", value_parser = bounded_duration)]
    reporting_interval: Duration,
    #[arg(long, default_value = "19")]
    samples_per_second: NonZeroU32,
    #[arg(long, default_value = "per-numa", value_parser = sharding)]
    sharding: ShardingStrategy,
    #[arg(long)]
    object: Option<PathBuf>,
    #[arg(long, value_delimiter = ',')]
    cpus: Vec<u32>,
    #[arg(long)]
    include_kernel_stacks: bool,
    #[arg(long)]
    symbols: bool,
    #[arg(long, default_value = "0s", value_parser = humantime::parse_duration)]
    consumer_delay: Duration,
    #[arg(long, default_value = "5s", value_parser = bounded_duration)]
    shutdown_timeout: Duration,
    #[arg(long, value_enum, default_value = "human")]
    output: Output,
    #[arg(long)]
    memory_estimate_only: bool,
}

fn bounded_duration(value: &str) -> Result<Duration, String> {
    let duration = humantime::parse_duration(value).map_err(|error| error.to_string())?;
    if duration.is_zero() || duration > Duration::from_secs(24 * 60 * 60) {
        return Err("duration must be greater than zero and at most 24 hours".to_owned());
    }
    Ok(duration)
}

fn sharding(value: &str) -> Result<ShardingStrategy, String> {
    match value {
        "single" => Ok(ShardingStrategy::Single),
        "per-numa" => Ok(ShardingStrategy::PerNuma),
        value => match value.strip_prefix("fixed:") {
            Some(count) => count
                .parse::<NonZeroUsize>()
                .map(|workers| ShardingStrategy::Fixed { workers })
                .map_err(|error| error.to_string()),
            None => Err("expected single, per-numa, or fixed:N".to_owned()),
        },
    }
}

#[derive(Default)]
struct Summary {
    windows: u64,
    represented_samples: u64,
    process_rows: u64,
    stack_rows: u64,
    maximum_stack_depth: usize,
    snapshot_bytes: u64,
    start: Option<SystemTime>,
    end: Option<SystemTime>,
}

impl Summary {
    fn consume(&mut self, snapshot: ProfileSnapshot) -> Result<(), ProfilerError> {
        snapshot.validate()?;
        self.windows = checked_add(self.windows, 1)?;
        self.represented_samples = checked_add(
            self.represented_samples,
            snapshot.statistics.samples_in_snapshots,
        )?;
        self.process_rows = checked_add(self.process_rows, snapshot.processes.len() as u64)?;
        self.stack_rows = checked_add(self.stack_rows, snapshot.stacks.len() as u64)?;
        self.maximum_stack_depth = self.maximum_stack_depth.max(
            snapshot
                .stacks
                .iter()
                .map(|stack| stack.locations.len())
                .max()
                .unwrap_or(0),
        );
        self.snapshot_bytes = checked_add(self.snapshot_bytes, snapshot.logical_bytes() as u64)?;
        self.start = Some(self.start.map_or(snapshot.window_start, |start| {
            start.min(snapshot.window_start)
        }));
        self.end = Some(
            self.end
                .map_or(snapshot.window_end, |end| end.max(snapshot.window_end)),
        );
        Ok(())
    }
}

fn checked_add(left: u64, right: u64) -> Result<u64, ProfilerError> {
    left.checked_add(right)
        .ok_or_else(|| ProfilerError::InternalInvariant("capture summary overflow".to_owned()))
}

fn estimate_json(estimate: &MemoryEstimate) -> Result<serde_json::Value, ProfilerError> {
    Ok(serde_json::json!({
        "total_bytes": estimate.total_bytes()?,
        "initialization_bytes": estimate.initialization,
        "running_bytes": estimate.running_bytes()?,
        "perf_buffers": estimate.perf_buffers,
        "kernel_maps": estimate.kernel_maps,
        "workers": estimate.workers,
        "active_aggregation": estimate.active_aggregation,
        "pending_windows": estimate.pending_windows,
        "finalization": estimate.finalization,
        "completed_snapshots": estimate.completed_snapshots,
        "scratch": estimate.scratch,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
    if arguments.consumer_delay > arguments.duration {
        return Err(ProfilerError::InvalidConfiguration {
            field: "consumer_delay",
            reason: "must not exceed capture duration".to_owned(),
        }
        .into());
    }
    let config = ProfilerConfig {
        samples_per_second: arguments.samples_per_second,
        reporting_interval: arguments.reporting_interval,
        sharding: arguments.sharding,
        include_kernel_stacks: arguments.include_kernel_stacks,
        cpu_selection: if arguments.cpus.is_empty() {
            CpuSelection::AllOnline
        } else {
            CpuSelection::Include(arguments.cpus)
        },
        program: ProgramConfig {
            object_path: arguments.object,
            ..ProgramConfig::default()
        },
        symbolization: SymbolizationConfig {
            enabled: arguments.symbols,
        },
        ..ProfilerConfig::default()
    }
    .validate()?
    .into_inner();
    let estimate = config.memory_estimate()?;
    if arguments.memory_estimate_only {
        println!("{}", estimate_json(&estimate)?);
        return Ok(());
    }
    let started = Instant::now();
    let deadline = started.checked_add(arguments.duration).ok_or_else(|| {
        ProfilerError::InvalidConfiguration {
            field: "duration",
            reason: "deadline overflow".to_owned(),
        }
    })?;
    let (profiler, receiver) = ProfilerBuilder::new(config).start()?;
    let mut summary = Summary::default();
    while Instant::now() < deadline {
        match receiver.recv_timeout(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100)),
        ) {
            Ok(snapshot) => {
                summary.consume(snapshot)?;
                std::thread::sleep(
                    arguments
                        .consumer_delay
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(ProfilerError::SnapshotReceiveTimeout) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let stopping = Instant::now();
    let report = profiler.shutdown(
        stopping
            .checked_add(arguments.shutdown_timeout)
            .ok_or_else(|| ProfilerError::InvalidConfiguration {
                field: "shutdown_timeout",
                reason: "deadline overflow".to_owned(),
            })?,
    )?;
    let shutdown_ns = stopping.elapsed().as_nanos();
    loop {
        match receiver.try_recv() {
            Ok(Some(snapshot)) => summary.consume(snapshot)?,
            Ok(None) | Err(ProfilerError::SnapshotConsumerDisconnected) => break,
            Err(error) => return Err(error.into()),
        }
    }
    let stats = &report.statistics;
    let epoch = |time: Option<SystemTime>| {
        time.and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs_f64())
    };
    let output = serde_json::json!({
        "schema": "otel-ebpf-profiler-summary-v1",
        "status": if report.cleanup_complete && report.failure.is_none() { "ok" } else { "failed" },
        "duration_seconds": started.elapsed().as_secs_f64(),
        "window_start_unix_seconds": epoch(summary.start),
        "window_end_unix_seconds": epoch(summary.end),
        "snapshots_consumed": summary.windows,
        "samples_consumed": summary.represented_samples,
        "raw_events_received": stats.raw_events_received,
        "samples_accepted": stats.samples_accepted,
        "samples_aggregated": stats.samples_aggregated,
        "samples_in_snapshots": stats.samples_in_snapshots,
        "userspace_samples_dropped": stats.userspace_dropped(),
        "kernel_lost_events": stats.kernel_lost_events,
        "kernel_output_failures": stats.kernel_output_failures,
        "sampling_periods_attempted": stats.sampling_periods_attempted,
        "idle_samples_skipped": stats.idle_samples_skipped,
        "unobserved_samples": stats.unobserved_samples,
        "process_rows_consumed": summary.process_rows,
        "stack_rows_consumed": summary.stack_rows,
        "maximum_stack_depth": summary.maximum_stack_depth,
        "mapping_cache_occupancy": stats.mapping_cache_occupancy,
        "symbol_cache_occupancy": stats.symbol_cache_occupancy,
        "cache_evictions": stats.cache_evictions,
        "capacity_rejections": stats.capacity_rejections,
        "worker_wakeups": stats.worker_wakeups,
        "reporting_duration_ns_peak": stats.reporting_duration_ns,
        "event_to_aggregation_ns_max": stats.event_to_aggregation_ns_max,
        "snapshot_logical_bytes_total": summary.snapshot_bytes,
        "snapshot_logical_bytes_peak": stats.snapshot_logical_bytes,
        "snapshots_dropped": stats.snapshots_dropped,
        "shutdown_ns": shutdown_ns,
        "shutdown_losses": stats.shutdown_losses,
        "shutdown_timeouts": stats.shutdown_timeouts,
        "workers_joined": report.workers_joined,
        "cleanup_complete": report.cleanup_complete,
        "affinity_failures": stats.affinity_failures,
        "attached_cpus": stats.attached_cpus,
        "unavailable_cpus": stats.unavailable_cpus,
        "memory_estimate": estimate_json(&estimate)?,
    });
    match arguments.output {
        Output::Json => println!("{output}"),
        Output::Human => {
            println!(
                "Windows: {} ({:?} to {:?})",
                summary.windows, summary.start, summary.end
            );
            println!(
                "Samples: {} consumed, {} aggregated, {} userspace dropped",
                summary.represented_samples,
                stats.samples_aggregated,
                stats.userspace_dropped()
            );
            println!(
                "Kernel: {} attempts, {} output failures, {} lost notifications (overlapping counters)",
                stats.sampling_periods_attempted,
                stats.kernel_output_failures,
                stats.kernel_lost_events
            );
            println!(
                "Rows: {} process, {} stack; maximum depth: {}",
                summary.process_rows, summary.stack_rows, summary.maximum_stack_depth
            );
            println!(
                "Cache peaks: {} mappings, {} symbols; snapshot bytes: {} total / {} peak",
                stats.mapping_cache_occupancy,
                stats.symbol_cache_occupancy,
                summary.snapshot_bytes,
                stats.snapshot_logical_bytes
            );
            println!(
                "Shutdown: {} workers joined, cleanup={}, loss={}, time={} ns",
                report.workers_joined, report.cleanup_complete, stats.shutdown_losses, shutdown_ns
            );
        }
    }
    if let Some(failure) = report.failure {
        return Err(ProfilerError::WorkerFailed {
            shard: failure.shard,
            reason: failure.reason,
        }
        .into());
    }
    if !report.cleanup_complete {
        return Err(ProfilerError::InternalInvariant("cleanup was incomplete".to_owned()).into());
    }
    Ok(())
}
