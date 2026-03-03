// df-local-perf — local-only df_engine performance and memory analysis harness
//
// NOT intended for CI or check-in. Add `/local-perf/` to .gitignore.
//
// Usage:
//   cargo run --release -- throughput --cores 4 --duration 30
//   cargo run --release -- latency   --rate 500000 --cores 4
//   cargo run --release -- leak      --cores 2 --duration 120 --leak-threshold-mb 10
//   cargo run --release -- throughput --threshold-msgs 10000000  (stop after 10M msgs)
//   cargo run --release -- scaling   --core-sweep 1,2,4,8 --rate-sweep 50000,200000
//   cargo run --release -- burst     --cores 4 --duration 40
//
// With heap profiling (re-compile required):
//   cargo run --release --features dhat-heap -- throughput --cores 4
//
// With artifacts directory (samples.csv + summary.csv written per run):
//   cargo run --release -- --artifacts-dir /tmp/perf-out scaling --core-sweep 1,2,4
//
// Key flags:
//   --no-tui              Plain text output (useful for piping / logging)
//   --json-out FILE       Write final result to a JSON file
//   --artifacts-dir DIR   Write samples.csv + summary.csv here (mirrors scale_lab.sh output)
//   --threshold-msgs N    Stop after N total messages received (0 = duration only)
//   --leak-threshold-mb F Flag a run as leaking when net heap growth > F MB (default: 64)
//   --core-sweep CSV      Scaling scenario core counts, e.g. "1,2,4,8"
//   --rate-sweep CSV      Scaling scenario rates/core, e.g. "50000,100000,200000"
//   --processors N        Number of passthrough processor stages [default: 1]
//   --channel-cap N       Channel buffer depth [default: 256]
//   --msg-size N          Payload bytes per message [default: 1024]

#![allow(dead_code)]

// ── dhat heap profiler (compile-time opt-in) ─────────────────────────────────
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

// ── jemalloc (default on Unix; off on Windows; disabled when dhat-heap is active) ──
#[cfg(all(feature = "jemalloc", not(windows), not(feature = "dhat-heap")))]
#[global_allocator]
static ALLOC_JEMALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod harness;
mod memory;
mod metrics;
mod scenarios;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "df-local-perf",
    about = "df_engine local performance & memory analysis harness",
    long_about = "
Thread-per-core, instance-per-core pipeline harness that mirrors the df_engine's
exact runtime architecture:
  • current_thread Tokio runtime per OS thread
  • LocalSet for !Send async tasks (engine's 'local' node mode)
  • otap-df-channel::mpsc (engine's lock-free channels)
  • core_affinity CPU pinning

Scenarios:
  throughput  Max message rate — no rate limiting, generator hammers the channel
  latency     Controlled rate with HDR histogram latency measurement
  leak        Long-run heap tracking with linear regression leak detection
  scaling     Doubling core sweep (1, 2, 4, … N cores) — shows linear scaling
  burst       Alternating burst/quiet cycles for backpressure testing
"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Number of pipeline instances (cores) to run in parallel.
    #[arg(long, global = true, default_value_t = num_cpus())]
    pub cores: usize,

    /// Test duration in seconds.
    #[arg(long, global = true, default_value_t = 30)]
    pub duration: u64,

    /// Target event rate per core (msgs/sec). 0 = unlimited.
    #[arg(long, global = true)]
    pub rate: Option<u32>,

    /// Payload bytes per message (simulates a telemetry batch).
    #[arg(long, global = true, default_value_t = 1024)]
    pub msg_size: usize,

    /// Channel buffer depth (engine default = 256).
    #[arg(long, global = true, default_value_t = 256)]
    pub channel_cap: usize,

    /// Number of passthrough processor stages (0 = generator → sink directly).
    #[arg(long, global = true, default_value_t = 1)]
    pub processors: usize,

    /// Disable the ratatui TUI — print plain text instead.
    #[arg(long, global = true)]
    pub no_tui: bool,

    /// Write final scenario result to this JSON file.
    #[arg(long, global = true, value_name = "FILE")]
    pub json_out: Option<String>,

    /// Write per-run samples.csv + summary.csv to this directory.
    /// Mirrors the artifact layout from df_engine_scale_lab.sh.
    #[arg(long, global = true, value_name = "DIR")]
    pub artifacts_dir: Option<String>,

    /// Stop the run early when total received messages reach this count.
    /// 0 = disabled (run until --duration). Mirrors --threshold-events in scale_lab.sh.
    #[arg(long, global = true, default_value_t = 0)]
    pub threshold_msgs: u64,

    /// Flag a run as a potential memory leak when net heap growth exceeds this many MB.
    /// Used alongside the linear-regression leak analysis. Default matches scale_lab.sh.
    #[arg(long, global = true, default_value_t = 64.0)]
    pub leak_threshold_mb: f64,

    /// Core counts for the scaling matrix (comma-separated, e.g. "1,2,4,8").
    /// Used by the `scaling` scenario. Overrides --cores when set.
    #[arg(long, global = true, value_name = "CSV")]
    pub core_sweep: Option<String>,

    /// Rate limits (msgs/sec/core) for the scaling matrix (comma-separated).
    /// When set, `scaling` runs every (core, rate) combination.
    #[arg(long, global = true, value_name = "CSV")]
    pub rate_sweep: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Maximum throughput — no rate limit, find the ceiling.
    Throughput,
    /// Latency distribution at controlled rate (uses HDR histogram).
    Latency,
    /// Long-running memory leak detection (jemalloc + linear regression).
    Leak,
    /// Scaling sweep — doubles core count from 1 to --cores.
    Scaling,
    /// Burst/quiet cycles — tests backpressure and recovery.
    Burst,
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    // Initialise dhat profiler if enabled.
    // dhat::Profiler must be alive for the entire program duration.
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    let cli = Cli::parse();

    // Sanity checks
    assert!(cli.cores >= 1, "--cores must be >= 1");
    assert!(cli.msg_size >= 1, "--msg-size must be >= 1");
    assert!(cli.channel_cap >= 1, "--channel-cap must be >= 1");

    print_banner(&cli);

    match &cli.command {
        Command::Throughput => scenarios::throughput(&cli),
        Command::Latency    => scenarios::latency(&cli),
        Command::Leak       => scenarios::leak(&cli),
        Command::Scaling    => scenarios::scaling(&cli),
        Command::Burst      => scenarios::burst(&cli),
    }
}

fn print_banner(cli: &Cli) {
    println!();
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║          df_engine local-perf harness                   ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  Architecture: thread-per-core + LocalSet + mpsc chan   ║");
    #[cfg(all(feature = "jemalloc", not(windows)))]
    println!("║  Allocator   : jemalloc (heap stats enabled)            ║");
    #[cfg(feature = "dhat-heap")]
    println!("║  Profiler    : dhat heap profiling active               ║");
    #[cfg(not(any(feature = "jemalloc", feature = "dhat-heap")))]
    println!("║  Allocator   : system (enable jemalloc for heap stats)  ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  Cores       : {}                                        ║", cli.cores);
    println!("║  Duration    : {}s                                       ║", cli.duration);
    println!("║  Processors  : {} passthrough stages                    ║", cli.processors);
    println!("║  Msg size    : {} bytes                                  ║", cli.msg_size);
    println!("║  Channel cap : {}                                        ║", cli.channel_cap);
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();
}
