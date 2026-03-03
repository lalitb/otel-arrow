// Local performance harness — metrics collection
//
// Shared atomic counters + HDR histogram for latency.
// All counters are 64-bit atomics so pipeline threads can update them
// without coordination while the TUI thread samples them.

use hdrhistogram::Histogram;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Per-instance counters (one set per core instance)
// ---------------------------------------------------------------------------

pub struct InstanceMetrics {
    /// Total messages produced by the generator on this core.
    pub generated: AtomicU64,
    /// Total messages that reached the sink on this core.
    pub received: AtomicU64,
    /// Messages dropped (channel-full back-pressure events).
    pub dropped: AtomicU64,
    /// Latency histogram: nanoseconds from message creation to sink receipt.
    /// Wrapped in Mutex so it is Send; the pipeline thread is the only writer.
    pub latency_ns: Mutex<Histogram<u64>>,
    /// CPU-time nanoseconds consumed by this instance (sampled at end).
    pub cpu_ns: AtomicU64,
}

impl Default for InstanceMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl InstanceMetrics {
    pub fn new() -> Self {
        // Three significant figures, max 60 s in nanoseconds
        let hist = Histogram::new_with_max(60_000_000_000, 3).expect("valid histogram params");
        InstanceMetrics {
            generated: AtomicU64::new(0),
            received: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            latency_ns: Mutex::new(hist),
            cpu_ns: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn record_latency(&self, ns: u64) {
        if let Ok(mut h) = self.latency_ns.lock() {
            let _ = h.record(ns.min(60_000_000_000));
        }
    }
}

// ---------------------------------------------------------------------------
// Global aggregate (across all instances)
// ---------------------------------------------------------------------------

pub struct GlobalMetrics {
    pub instances: Vec<Arc<InstanceMetrics>>,
    pub start: Instant,
}

impl GlobalMetrics {
    pub fn new(num_cores: usize) -> Self {
        GlobalMetrics {
            instances: (0..num_cores).map(|_| Arc::new(InstanceMetrics::new())).collect(),
            start: Instant::now(),
        }
    }

    /// Wall-clock elapsed since start.
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Total messages received across all instances since start.
    pub fn total_received(&self) -> u64 {
        self.instances
            .iter()
            .map(|m| m.received.load(Ordering::Relaxed))
            .sum()
    }

    /// Total messages generated across all instances since start.
    pub fn total_generated(&self) -> u64 {
        self.instances
            .iter()
            .map(|m| m.generated.load(Ordering::Relaxed))
            .sum()
    }

    pub fn total_dropped(&self) -> u64 {
        self.instances
            .iter()
            .map(|m| m.dropped.load(Ordering::Relaxed))
            .sum()
    }

    /// Instantaneous throughput (msgs/s) computed from a snapshot delta.
    pub fn throughput_from_snapshot(&self, prev: u64, prev_at: Instant) -> f64 {
        let now_count = self.total_received();
        let elapsed = prev_at.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            (now_count.saturating_sub(prev)) as f64 / elapsed
        } else {
            0.0
        }
    }

    /// Merge all per-instance histograms into a single histogram for reporting.
    pub fn merged_latency_histogram(&self) -> Histogram<u64> {
        let mut merged =
            Histogram::new_with_max(60_000_000_000, 3).expect("valid histogram params");
        for inst in &self.instances {
            if let Ok(h) = inst.latency_ns.lock() {
                let _ = merged.add(&*h);
            }
        }
        merged
    }
}

// ---------------------------------------------------------------------------
// Throughput history for sparkline display
// ---------------------------------------------------------------------------

/// Ring buffer of recent per-second throughput samples (for TUI sparkline).
pub struct ThroughputHistory {
    pub samples: Vec<f64>,
    capacity: usize,
}

impl ThroughputHistory {
    pub fn new(capacity: usize) -> Self {
        ThroughputHistory {
            samples: Vec::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, value: f64) {
        if self.samples.len() >= self.capacity {
            self.samples.remove(0);
        }
        self.samples.push(value);
    }

    pub fn max(&self) -> f64 {
        self.samples.iter().cloned().fold(0.0_f64, f64::max)
    }
}

// ---------------------------------------------------------------------------
// Snapshot — immutable point-in-time view for TUI rendering
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct MetricsSnapshot {
    pub elapsed: Duration,
    pub generated: u64,
    pub received: u64,
    pub dropped: u64,
    pub throughput: f64, // msgs/s
    pub p50_us: u64,
    pub p90_us: u64,
    pub p99_us: u64,
    pub p999_us: u64,
    pub max_us: u64,
    pub num_cores: usize,
}

impl MetricsSnapshot {
    pub fn capture(global: &GlobalMetrics, throughput: f64) -> Self {
        let hist = global.merged_latency_histogram();
        MetricsSnapshot {
            elapsed: global.elapsed(),
            generated: global.total_generated(),
            received: global.total_received(),
            dropped: global.total_dropped(),
            throughput,
            p50_us: hist.value_at_quantile(0.50) / 1_000,
            p90_us: hist.value_at_quantile(0.90) / 1_000,
            p99_us: hist.value_at_quantile(0.99) / 1_000,
            p999_us: hist.value_at_quantile(0.999) / 1_000,
            max_us: hist.max() / 1_000,
            num_cores: global.instances.len(),
        }
    }
}
