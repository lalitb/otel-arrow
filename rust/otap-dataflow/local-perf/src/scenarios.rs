// Local performance harness — scenario definitions
//
// Integrates the most useful ideas from df_engine_scale_lab.sh:
//   • --threshold-msgs  : stop early when N messages have been received
//   • --core-sweep × --rate-sweep matrix : full cross-product scaling runs
//   • samples.csv time series per run  : same columns as scale_lab.sh
//   • RSS tracking                     : mirrors `ps -o rss=` in scale_lab.sh
//   • --leak-threshold-mb              : absolute growth flag (+ regression)
//
// What's NOT ported: YAML config, HTTP admin API, heaptrack/massif, numactl.
// Those are real-engine integration concerns, not harness-level ones.

use crate::{
    Cli,
    harness::{InstanceConfig, spawn_instance},
    memory::{
        LeakAnalysis, MemorySample, MemoryTracker, SAMPLE_INTERVAL,
        format_bytes, sample_heap, sample_rss_bytes,
    },
    metrics::{GlobalMetrics, MetricsSnapshot, ThroughputHistory},
    tui::Tui,
};
use anyhow::Result;
use std::{
    fs,
    io::Write as IoWrite,
    num::NonZeroU32,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

// ---------------------------------------------------------------------------
// Final result of a single scenario run
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ScenarioResult {
    pub scenario: String,
    pub num_cores: usize,
    pub target_rate: Option<u32>,
    pub duration: Duration,
    pub peak_throughput: f64,
    pub avg_throughput: f64,
    pub snap: MetricsSnapshot,
    pub rss_growth_bytes: i64,
    pub leak: Option<LeakAnalysis>,
    /// True when net heap growth exceeded --leak-threshold-mb.
    pub leak_flag: bool,
    /// Why the run stopped: "duration_limit", "threshold_reached", or "user_quit".
    pub stop_reason: String,
}

impl ScenarioResult {
    pub fn print_summary(&self) {
        println!();
        println!("══════════════════════════════════════════════════════════");
        println!("  Scenario  : {}", self.scenario);
        println!("  Cores     : {}", self.num_cores);
        println!("  Duration  : {:.1}s", self.duration.as_secs_f64());
        println!("  Throughput: avg {:.2}M msg/s  peak {:.2}M msg/s",
            self.avg_throughput / 1e6, self.peak_throughput / 1e6);
        println!("  Generated : {}  Received: {}  Dropped: {}",
            self.snap.generated, self.snap.received, self.snap.dropped);
        println!("  Latency   : P50={}µs  P99={}µs  P99.9={}µs  Max={}µs",
            self.snap.p50_us, self.snap.p99_us, self.snap.p999_us, self.snap.max_us);
        if let Some(ref la) = self.leak {
            let flag = if self.leak_flag { " ← FLAGGED" } else { "" };
            println!("  Heap      : {} | slope {:.1} KB/s | R²={:.3}{}",
                la.verdict,
                la.slope_bytes_per_sec / 1024.0,
                la.r_squared,
                flag);
        } else {
            println!("  Heap      : (enable jemalloc feature for heap analysis)");
        }
        println!("  RSS growth: {:+} MB",
            self.rss_growth_bytes / (1024 * 1024));
        println!("══════════════════════════════════════════════════════════");
    }
}

// ---------------------------------------------------------------------------
// RunConfig — what run_scenario() uses internally
// ---------------------------------------------------------------------------

struct RunConfig {
    scenario: String,
    num_cores: usize,
    num_processors: usize,
    msg_size: usize,
    channel_cap: usize,
    rate: Option<NonZeroU32>,
    duration: Duration,
    threshold_msgs: u64,
    measure_latency: bool,
    show_tui: bool,
    leak_threshold_mb: f64,
    artifacts_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Core monitoring loop — shared by all scenarios
// ---------------------------------------------------------------------------

fn run_scenario(cfg: RunConfig) -> Result<ScenarioResult> {
    let stop = Arc::new(AtomicBool::new(false));
    let global = Arc::new(GlobalMetrics::new(cfg.num_cores));

    let mut handles: Vec<JoinHandle<()>> = Vec::with_capacity(cfg.num_cores);
    for core_id in 0..cfg.num_cores {
        let inst_cfg = InstanceConfig {
            core_id,
            num_processors: cfg.num_processors,
            msg_size_bytes: cfg.msg_size,
            channel_capacity: cfg.channel_cap,
            target_rate: cfg.rate,
            measure_latency: cfg.measure_latency,
            stop: stop.clone(),
        };
        let metrics = global.instances[core_id].clone();
        handles.push(spawn_instance(inst_cfg, metrics));
    }

    // ── Prepare artifacts directory and samples.csv ───────────────────────
    // File layout mirrors df_engine_scale_lab.sh's per-run directory.
    let mut samples_csv: Option<fs::File> = None;
    if let Some(ref dir) = cfg.artifacts_dir {
        fs::create_dir_all(dir)?;
        let csv_path = dir.join("samples.csv");
        let mut f = fs::File::create(&csv_path)?;
        // Header matches scale_lab.sh samples.csv exactly for tool interoperability
        writeln!(f, "elapsed_s,received_total,received_rate_per_s,\
            dropped_total,alloc_bytes,resident_bytes,rss_bytes,\
            p50_us,p99_us,p999_us")?;
        samples_csv = Some(f);
    }

    // ── Monitoring loop (main thread) ─────────────────────────────────────
    let mut tui = if cfg.show_tui { Some(Tui::init()?) } else { None };
    let mut memory_tracker = MemoryTracker::default();
    let mut throughput_history = ThroughputHistory::new(120);

    let start = Instant::now();
    let mut prev_count = 0u64;
    let mut prev_at = start;
    let mut peak_tp = 0.0_f64;
    let mut tp_sum = 0.0_f64;
    let mut tp_n = 0u64;
    let mut stop_reason = "duration_limit";

    // RSS at start for growth calculation
    let rss_start = sample_rss_bytes();

    while start.elapsed() < cfg.duration {
        std::thread::sleep(SAMPLE_INTERVAL);

        // ── Throughput ───────────────────────────────────────────────────
        let now_count = global.total_received();
        let tp = global.throughput_from_snapshot(prev_count, prev_at);
        prev_count = now_count;
        prev_at = Instant::now();
        throughput_history.push(tp);
        if tp > peak_tp { peak_tp = tp; }
        tp_sum += tp;
        tp_n += 1;

        // ── Memory samples ───────────────────────────────────────────────
        // Only push heap samples when jemalloc data is actually available.
        // Pushing allocated=0 on non-jemalloc builds would produce a flat
        // regression that returns a spurious "CLEAN" verdict.
        let rss_now = sample_rss_bytes();
        let (alloc, resident) = if let Some(heap) = sample_heap() {
            memory_tracker.push(MemorySample {
                secs_since_start: start.elapsed().as_secs_f64(),
                allocated: heap.allocated,
                resident: heap.resident,
                rss_bytes: rss_now,
            });
            (heap.allocated, heap.resident)
        } else {
            // No jemalloc — RSS is still tracked for CSV but no heap regression.
            (0, 0)
        };

        let leak = memory_tracker.leak_analysis();
        let snap = MetricsSnapshot::capture(&global, tp);

        // ── Write samples.csv row ─────────────────────────────────────────
        if let Some(ref mut f) = samples_csv {
            writeln!(
                f,
                "{:.3},{},{:.0},{},{},{},{},{},{},{}",
                start.elapsed().as_secs_f64(),
                snap.received,
                tp,
                snap.dropped,
                alloc,
                resident,
                rss_now,
                snap.p50_us,
                snap.p99_us,
                snap.p999_us,
            )?;
        }

        // ── TUI / plain text ──────────────────────────────────────────────
        if let Some(ref mut tui) = tui {
            if tui.draw(&cfg.scenario, &snap, &throughput_history, &memory_tracker, leak.as_ref())? {
                stop_reason = "user_quit";
                break;
            }
        } else {
            eprint!(
                "\r  [{:5.1}s]  tp: {:7.2}M msg/s  recv: {}  drop: {}  alloc: {}  rss: {}  ",
                start.elapsed().as_secs_f64(),
                tp / 1e6,
                snap.received,
                snap.dropped,
                format_bytes(alloc),
                format_bytes(rss_now),
            );
        }

        // ── Threshold stop (mirrors --threshold-events in scale_lab.sh) ──
        if cfg.threshold_msgs > 0 && now_count >= cfg.threshold_msgs {
            stop_reason = "threshold_reached";
            break;
        }
    }

    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.join(); // wait for each worker thread to exit cleanly
    }

    if tui.is_none() { eprintln!(); }

    let avg_tp = if tp_n > 0 { tp_sum / tp_n as f64 } else { 0.0 };
    let final_snap = MetricsSnapshot::capture(&global, avg_tp);
    let leak = memory_tracker.leak_analysis();
    let rss_end = sample_rss_bytes();
    let rss_growth = rss_end as i64 - rss_start as i64;

    let leak_flag = leak.as_ref().map_or(false, |la| {
        (la.net_growth_bytes as f64 / (1024.0 * 1024.0)) > cfg.leak_threshold_mb
    });

    Ok(ScenarioResult {
        scenario: cfg.scenario,
        num_cores: cfg.num_cores,
        target_rate: cfg.rate.map(|r| r.get()),
        duration: start.elapsed(),
        peak_throughput: peak_tp,
        avg_throughput: avg_tp,
        snap: final_snap,
        rss_growth_bytes: rss_growth,
        leak,
        leak_flag,
        stop_reason: stop_reason.to_string(),
    })
}

// ---------------------------------------------------------------------------
// CSV helpers for multi-run summary (mirrors scale_lab.sh summary.csv)
// ---------------------------------------------------------------------------

fn summary_csv_header() -> &'static str {
    "run_id,cores,rate_per_core,observed_seconds,received_events,\
     throughput_events_per_sec,memory_growth_mb,rss_growth_mb,\
     leak_rate_kb_per_sec,r_squared,leak_flag,stop_reason"
}

fn result_to_csv_row(r: &ScenarioResult) -> String {
    let (growth_mb, rate_kb, r2) = r.leak.as_ref().map_or(
        (0.0_f64, 0.0_f64, 0.0_f64),
        |la| (
            la.net_growth_bytes as f64 / (1024.0 * 1024.0),
            la.slope_bytes_per_sec / 1024.0,
            la.r_squared,
        ),
    );
    let rss_growth_mb = r.rss_growth_bytes as f64 / (1024.0 * 1024.0);
    format!(
        "{},{},{},{:.1},{},{:.0},{:.3},{:.3},{:.3},{:.4},{},{}",
        r.scenario,
        r.num_cores,
        r.target_rate.unwrap_or(0),
        r.duration.as_secs_f64(),
        r.snap.received,
        r.avg_throughput,
        growth_mb,
        rss_growth_mb,
        rate_kb,
        r2,
        r.leak_flag,
        r.stop_reason,
    )
}

// ---------------------------------------------------------------------------
// Scenario: Maximum Throughput
// ---------------------------------------------------------------------------

pub fn throughput(cli: &Cli) -> Result<()> {
    println!("▶ Throughput — {} cores, {}s, {} stages, {} B/msg{}",
        cli.cores, cli.duration, cli.processors, cli.msg_size,
        if cli.threshold_msgs > 0 { format!(", stop at {}M msgs", cli.threshold_msgs / 1_000_000) } else { String::new() });

    let artifacts = artifacts_subdir(cli, "throughput");
    let result = run_scenario(RunConfig {
        scenario: "throughput".into(),
        num_cores: cli.cores,
        num_processors: cli.processors,
        msg_size: cli.msg_size,
        channel_cap: cli.channel_cap,
        rate: None,
        duration: Duration::from_secs(cli.duration),
        threshold_msgs: cli.threshold_msgs,
        measure_latency: false,
        show_tui: !cli.no_tui,
        leak_threshold_mb: cli.leak_threshold_mb,
        artifacts_dir: artifacts,
    })?;

    result.print_summary();
    maybe_write_json(cli, &result)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario: Latency Distribution
// ---------------------------------------------------------------------------

pub fn latency(cli: &Cli) -> Result<()> {
    let rate = NonZeroU32::new(cli.rate.unwrap_or(100_000))
        .expect("rate must be non-zero");
    println!("▶ Latency — {} cores, {} msg/s/core, {}s", cli.cores, rate, cli.duration);

    let artifacts = artifacts_subdir(cli, "latency");
    let result = run_scenario(RunConfig {
        scenario: "latency".into(),
        num_cores: cli.cores,
        num_processors: cli.processors,
        msg_size: cli.msg_size,
        channel_cap: cli.channel_cap,
        rate: Some(rate),
        duration: Duration::from_secs(cli.duration),
        threshold_msgs: cli.threshold_msgs,
        measure_latency: true,
        show_tui: !cli.no_tui,
        leak_threshold_mb: cli.leak_threshold_mb,
        artifacts_dir: artifacts,
    })?;

    result.print_summary();
    maybe_write_json(cli, &result)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario: Memory Leak Detection
// ---------------------------------------------------------------------------

pub fn leak(cli: &Cli) -> Result<()> {
    let duration = cli.duration.max(60);
    println!("▶ Leak — {} cores, {}s (min 60s), threshold {}MB",
        cli.cores, duration, cli.leak_threshold_mb);

    let artifacts = artifacts_subdir(cli, "leak");
    let result = run_scenario(RunConfig {
        scenario: "leak".into(),
        num_cores: cli.cores,
        num_processors: cli.processors,
        msg_size: cli.msg_size,
        channel_cap: cli.channel_cap,
        rate: NonZeroU32::new(cli.rate.unwrap_or(50_000)),
        duration: Duration::from_secs(duration),
        threshold_msgs: 0,
        measure_latency: false,
        show_tui: !cli.no_tui,
        leak_threshold_mb: cli.leak_threshold_mb,
        artifacts_dir: artifacts,
    })?;

    result.print_summary();
    maybe_write_json(cli, &result)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario: Scaling sweep  (core-sweep × rate-sweep matrix)
//
//   Mirrors df_engine_scale_lab.sh's double-loop over (cores, rates).
//   When --core-sweep and --rate-sweep are given, runs every combination.
//   Falls back to doubling sweep from 1..--cores when not set.
// ---------------------------------------------------------------------------

pub fn scaling(cli: &Cli) -> Result<()> {
    let core_counts = parse_sweep_csv(cli.core_sweep.as_deref(), cli.cores);
    let rates = parse_rate_sweep(cli.rate_sweep.as_deref());

    if core_counts.is_empty() {
        anyhow::bail!(
            "--core-sweep '{}' produced no valid core counts (expected comma-separated integers ≥ 1)",
            cli.core_sweep.as_deref().unwrap_or("")
        );
    }
    if rates.is_empty() {
        anyhow::bail!(
            "--rate-sweep '{}' produced no valid rates (expected comma-separated integers ≥ 0)",
            cli.rate_sweep.as_deref().unwrap_or("")
        );
    }

    println!("▶ Scaling matrix — cores {:?} × rates {:?}, {}s/run",
        core_counts, rates.iter().map(|r| r.map_or(0, |r| r.get())).collect::<Vec<_>>(), cli.duration);
    println!();

    // Summary CSV for all runs
    let summary_path = cli.artifacts_dir.as_ref().map(|d| {
        let p = PathBuf::from(d).join("summary.csv");
        p
    });
    if let Some(ref path) = summary_path {
        fs::create_dir_all(path.parent().unwrap())?;
        let mut f = fs::File::create(path)?;
        writeln!(f, "{}", summary_csv_header())?;
    }

    let mut all_results: Vec<ScenarioResult> = Vec::new();

    for &rate in &rates {
        let rate_label = rate.map_or("unlimited".to_string(), |r| format!("{}/s", r));
        for &n in &core_counts {
            let run_id = format!("c{}_r{}", n, rate.map_or(0, |r| r.get()));
            let artifacts = cli.artifacts_dir.as_ref().map(|d| PathBuf::from(d).join(&run_id));

            let r = run_scenario(RunConfig {
                scenario: run_id.clone(),
                num_cores: n,
                num_processors: cli.processors,
                msg_size: cli.msg_size,
                channel_cap: cli.channel_cap,
                rate,
                duration: Duration::from_secs(cli.duration),
                threshold_msgs: cli.threshold_msgs,
                measure_latency: false,
                show_tui: false,
                leak_threshold_mb: cli.leak_threshold_mb,
                artifacts_dir: artifacts,
            })?;

            let base_tp = all_results.first()
                .filter(|b| b.target_rate == r.target_rate)
                .map(|b| b.avg_throughput)
                .unwrap_or(r.avg_throughput);
            let efficiency = if n > 1 { r.avg_throughput / (base_tp * n as f64) * 100.0 } else { 100.0 };
            let leak_str = if r.leak_flag { " ⚠LEAK" } else { "" };

            println!(
                "  {:2} cores  rate={:>10}  avg {:7.2}M msg/s  peak {:7.2}M msg/s  eff {:.0}%  rss {:+.1}MB{}",
                n, rate_label,
                r.avg_throughput / 1e6,
                r.peak_throughput / 1e6,
                efficiency,
                r.rss_growth_bytes as f64 / (1024.0 * 1024.0),
                leak_str,
            );

            // Append to summary CSV
            if let Some(ref path) = summary_path {
                if let Ok(mut f) = fs::OpenOptions::new().append(true).open(path) {
                    writeln!(f, "{}", result_to_csv_row(&r))?;
                }
            }

            all_results.push(r);
        }
    }

    // ── Scaling efficiency chart per rate ─────────────────────────────────
    if rates.len() == 1 {
        println!();
        println!("  Scaling efficiency (ideal = 100%):");
        let base = all_results.first().map(|r| r.avg_throughput).unwrap_or(1.0);
        for r in &all_results {
            let ideal = base * r.num_cores as f64;
            let pct = r.avg_throughput / ideal * 100.0;
            let bar = "█".repeat((pct / 5.0) as usize);
            println!("  {:2} cores: [{:<20}] {:.1}%", r.num_cores, bar, pct);
        }
    }

    if let Some(ref path) = summary_path {
        println!("\n  Summary CSV: {}", path.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario: Burst — alternating high/quiet load cycles
// ---------------------------------------------------------------------------

pub fn burst(cli: &Cli) -> Result<()> {
    let burst_rate = NonZeroU32::new(1_000_000).unwrap();
    let quiet_rate = NonZeroU32::new(10_000).unwrap();
    let cycle = Duration::from_secs(5);
    let total_cycles = ((cli.duration / 5) as usize).max(2);

    println!("▶ Burst — {} cores, {} cycles of 5s (burst/quiet)", cli.cores, total_cycles);

    for idx in 0..total_cycles {
        let is_burst = idx % 2 == 0;
        let phase = if is_burst { "BURST" } else { "quiet" };
        let artifacts = cli.artifacts_dir.as_ref().map(|d| {
            PathBuf::from(d).join(format!("burst_cycle{}", idx))
        });

        let r = run_scenario(RunConfig {
            scenario: format!("burst/{phase}"),
            num_cores: cli.cores,
            num_processors: cli.processors,
            msg_size: cli.msg_size,
            channel_cap: cli.channel_cap,
            rate: if is_burst { Some(burst_rate) } else { Some(quiet_rate) },
            duration: cycle,
            threshold_msgs: 0,
            measure_latency: is_burst,
            show_tui: false,
            leak_threshold_mb: cli.leak_threshold_mb,
            artifacts_dir: artifacts,
        })?;

        println!("  Cycle {}/{}: {}  tp={:.2}M msg/s  drop={}  P99={}µs{}",
            idx + 1, total_cycles, phase,
            r.avg_throughput / 1e6,
            r.snap.dropped,
            r.snap.p99_us,
            if r.leak_flag { "  ⚠LEAK" } else { "" });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse "1,2,4,8" → [1, 2, 4, 8], or generate a doubling sweep up to max.
fn parse_sweep_csv(csv: Option<&str>, max: usize) -> Vec<usize> {
    if let Some(s) = csv {
        s.split(',')
            .filter_map(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n >= 1)
            .collect()
    } else {
        // Default: doubling sweep 1, 2, 4, … max
        let mut v = Vec::new();
        let mut n = 1usize;
        while n <= max { v.push(n); n *= 2; }
        if v.last() != Some(&max) { v.push(max); }
        v
    }
}

/// Parse "50000,100000" → [Some(50000), Some(100000)], or [None] for unlimited.
fn parse_rate_sweep(csv: Option<&str>) -> Vec<Option<NonZeroU32>> {
    match csv {
        None => vec![None], // unlimited
        Some(s) => s.split(',')
            .filter_map(|v| v.trim().parse::<u32>().ok())
            .map(|r| if r == 0 { None } else { NonZeroU32::new(r) })
            .collect(),
    }
}

/// If --artifacts-dir is set, return a subdirectory path for this scenario.
fn artifacts_subdir(cli: &Cli, scenario: &str) -> Option<PathBuf> {
    cli.artifacts_dir.as_ref().map(|d| PathBuf::from(d).join(scenario))
}

fn maybe_write_json(cli: &Cli, result: &ScenarioResult) -> Result<()> {
    if let Some(path) = &cli.json_out {
        let (growth_mb, rate_kb, r2, verdict) = result.leak.as_ref().map_or(
            (0.0, 0.0, 0.0, "no_data".to_string()),
            |la| (
                la.net_growth_bytes as f64 / (1024.0 * 1024.0),
                la.slope_bytes_per_sec / 1024.0,
                la.r_squared,
                format!("{}", la.verdict),
            ),
        );
        let obj = serde_json::json!({
            "scenario": result.scenario,
            "num_cores": result.num_cores,
            "rate_per_core": result.target_rate,
            "duration_secs": result.duration.as_secs_f64(),
            "throughput": {
                "avg_msgs_per_sec": result.avg_throughput,
                "peak_msgs_per_sec": result.peak_throughput,
            },
            "counts": {
                "generated": result.snap.generated,
                "received": result.snap.received,
                "dropped": result.snap.dropped,
            },
            "latency_us": {
                "p50": result.snap.p50_us,
                "p90": result.snap.p90_us,
                "p99": result.snap.p99_us,
                "p999": result.snap.p999_us,
                "max": result.snap.max_us,
            },
            "memory": {
                "heap_growth_mb": growth_mb,
                "heap_leak_rate_kb_per_sec": rate_kb,
                "r_squared": r2,
                "verdict": verdict,
                "leak_flag": result.leak_flag,
                "rss_growth_bytes": result.rss_growth_bytes,
            },
        });
        fs::write(path, serde_json::to_string_pretty(&obj)?)?;
        println!("  JSON: {}", path);
    }
    Ok(())
}
