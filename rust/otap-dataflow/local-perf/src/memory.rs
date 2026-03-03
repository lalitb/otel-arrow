// Local performance harness — memory tracking and leak detection
//
// Uses tikv-jemalloc-ctl to read live heap stats (allocated, resident, retained)
// and performs linear regression on time-series samples to detect monotonic growth
// (the signature of a memory leak).

use linreg::linear_regression;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Jemalloc stats (only available when the jemalloc feature is active)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct HeapStats {
    /// Total bytes allocated by the application and not yet freed.
    pub allocated: usize,
    /// Total bytes in active pages mapped by jemalloc (RSS-like, but per-allocator).
    pub resident: usize,
    /// Bytes held by jemalloc in retained (unmapped) memory.
    pub retained: usize,
    /// Total bytes ever allocated (monotonically increasing).
    pub cumulative_alloc: usize,
}

/// Refresh jemalloc's internal epoch and read heap stats.
///
/// Returns `None` on non-jemalloc builds (mimalloc / system allocator).
pub fn sample_heap() -> Option<HeapStats> {
    #[cfg(all(feature = "jemalloc", not(windows)))]
    {
        // Advance the jemalloc epoch to flush per-thread caches into global stats.
        tikv_jemalloc_ctl::epoch::mib().ok()?.advance().ok()?;
        Some(HeapStats {
            allocated: tikv_jemalloc_ctl::stats::allocated::mib().ok()?.read().ok()?,
            resident: tikv_jemalloc_ctl::stats::resident::mib().ok()?.read().ok()?,
            retained: tikv_jemalloc_ctl::stats::retained::mib().ok()?.read().ok()?,
            cumulative_alloc: 0,
        })
    }
    #[cfg(not(all(feature = "jemalloc", not(windows))))]
    {
        None
    }
}

// ---------------------------------------------------------------------------
// RSS — OS-level resident set size (catches mmap'd memory jemalloc misses)
//
// Mirrors what df_engine_scale_lab.sh reads via `ps -o rss=`.
// Works even without jemalloc (system allocator builds).
// ---------------------------------------------------------------------------

pub fn sample_rss_bytes() -> usize {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let pid = Pid::from_u32(std::process::id());
    let mut sys = System::new();
    // sysinfo 0.38: refresh_processes takes a ProcessesToUpdate + remove_dead flag
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), false);
    // memory() returns bytes in sysinfo 0.38
    sys.process(pid).map(|p| p.memory() as usize).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Memory time series + linear regression leak detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct MemorySample {
    pub secs_since_start: f64,
    /// jemalloc allocated bytes (None when jemalloc feature not active).
    pub allocated: usize,
    /// jemalloc resident bytes.
    pub resident: usize,
    /// OS-level RSS in bytes (always available; via sysinfo).
    pub rss_bytes: usize,
}

/// Accumulates memory samples over a run and provides leak analysis.
#[derive(Default)]
pub struct MemoryTracker {
    pub samples: Vec<MemorySample>,
}

impl MemoryTracker {
    pub fn push(&mut self, sample: MemorySample) {
        self.samples.push(sample);
    }

    /// Returns `Some((slope_bytes_per_sec, r_squared))` when ≥ 3 samples exist.
    ///
    /// A positive slope means allocated memory is growing over time.
    /// Rule of thumb: slope > ~10 KB/s sustained is suspicious.
    pub fn leak_analysis(&self) -> Option<LeakAnalysis> {
        if self.samples.len() < 3 {
            return None;
        }
        let xs: Vec<f64> = self.samples.iter().map(|s| s.secs_since_start).collect();
        let ys: Vec<f64> = self.samples.iter().map(|s| s.allocated as f64).collect();

        // linreg::linear_regression returns (slope, intercept)
        let (slope, intercept) = linear_regression(&xs, &ys).ok()?;

        // Compute R² manually
        let mean_y = ys.iter().sum::<f64>() / ys.len() as f64;
        let ss_tot: f64 = ys.iter().map(|y| (y - mean_y).powi(2)).sum();
        let ss_res: f64 = xs
            .iter()
            .zip(ys.iter())
            .map(|(x, y): (&f64, &f64)| {
                let predicted: f64 = slope * x + intercept;
                (y - predicted).powi(2)
            })
            .sum();
        let r_squared = if ss_tot > 0.0 {
            1.0 - (ss_res / ss_tot)
        } else {
            0.0
        };

        let baseline = self.samples.first().map(|s| s.allocated).unwrap_or(0);
        let latest = self.samples.last().map(|s| s.allocated).unwrap_or(0);
        let growth = latest.saturating_sub(baseline);

        Some(LeakAnalysis {
            slope_bytes_per_sec: slope,
            r_squared,
            baseline_bytes: baseline,
            current_bytes: latest,
            net_growth_bytes: growth,
            verdict: classify_leak(slope, r_squared, growth),
        })
    }

    /// Returns samples for TUI sparkline (allocated bytes history).
    pub fn allocated_history(&self) -> Vec<u64> {
        self.samples.iter().map(|s| s.allocated as u64).collect()
    }

    pub fn resident_history(&self) -> Vec<u64> {
        self.samples.iter().map(|s| s.resident as u64).collect()
    }
}

#[derive(Debug, Clone)]
pub struct LeakAnalysis {
    /// How many bytes per second the allocator says is live (linear regression slope).
    pub slope_bytes_per_sec: f64,
    /// Goodness-of-fit for the linear model (0–1). High R² = consistent growth.
    pub r_squared: f64,
    /// Allocated bytes at start of observation window.
    pub baseline_bytes: usize,
    /// Latest observed allocated bytes.
    pub current_bytes: usize,
    /// Total byte growth over the observation window.
    pub net_growth_bytes: usize,
    pub verdict: LeakVerdict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeakVerdict {
    Clean,
    Suspicious,
    Likely,
}

impl std::fmt::Display for LeakVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeakVerdict::Clean => write!(f, "✓ CLEAN"),
            LeakVerdict::Suspicious => write!(f, "⚠ SUSPICIOUS"),
            LeakVerdict::Likely => write!(f, "✗ LIKELY LEAK"),
        }
    }
}

fn classify_leak(slope: f64, r_squared: f64, net_growth: usize) -> LeakVerdict {
    // Thresholds tuned for a busy pipeline test:
    // - slope > 1 MB/s AND R² > 0.9  → almost certainly a leak
    // - slope > 100 KB/s OR net_growth > 10 MB → suspicious
    if slope > 1_000_000.0 && r_squared > 0.9 {
        LeakVerdict::Likely
    } else if slope > 100_000.0 || net_growth > 10_000_000 {
        LeakVerdict::Suspicious
    } else {
        LeakVerdict::Clean
    }
}

// ---------------------------------------------------------------------------
// Human-readable formatting helpers
// ---------------------------------------------------------------------------

pub fn format_bytes(b: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * KB;
    const GB: usize = 1024 * MB;
    if b >= GB {
        format!("{:.2} GB", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.2} MB", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.1} KB", b as f64 / KB as f64)
    } else {
        format!("{} B", b)
    }
}

pub fn format_bytes_per_sec(bps: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * KB;
    if bps.abs() >= MB {
        format!("{:+.2} MB/s", bps / MB)
    } else if bps.abs() >= KB {
        format!("{:+.1} KB/s", bps / KB)
    } else {
        format!("{:+.0} B/s", bps)
    }
}

// ---------------------------------------------------------------------------
// Sampling interval used by TUI and leak scenarios
// ---------------------------------------------------------------------------

pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
