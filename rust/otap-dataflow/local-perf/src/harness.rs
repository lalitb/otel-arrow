// Local performance harness — core pipeline engine
//
// Implements the df_engine's exact architectural pattern:
//   • One OS thread per core        (std::thread::spawn)
//   • One current_thread runtime    (Builder::new_current_thread — engine's per-core runtime)
//   • One LocalSet per runtime      (!Send futures — engine's "local" node mode)
//   • Three async tasks per inst:   Generator → Passthrough(×N) → Sink
//   • Core affinity                 (core_affinity crate)
//
// Channel layer: otap-df-channel::mpsc — the lock-free MPSC the engine uses
// for local (non-shared) node connections.

use crate::metrics::InstanceMetrics;
use bytes::Bytes;
use governor::{Quota, RateLimiter};
use otap_df_channel::mpsc;
use std::{
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use tokio::task::LocalSet;

// ---------------------------------------------------------------------------
// Tagged message: payload + creation timestamp for latency measurement
// ---------------------------------------------------------------------------

type Msg = (Bytes, Instant);

// ---------------------------------------------------------------------------
// Instance configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct InstanceConfig {
    /// Core index (0-based). Maps to a physical CPU via core_affinity.
    pub core_id: usize,
    /// Number of passthrough processor stages between generator and sink.
    pub num_processors: usize,
    /// Payload size in bytes per message. Simulates a telemetry batch size.
    pub msg_size_bytes: usize,
    /// Channel buffer depth. Engine default is 256.
    pub channel_capacity: usize,
    /// Optional rate cap (msgs/sec per core). `None` = run as fast as possible.
    pub target_rate: Option<NonZeroU32>,
    /// Record individual latency samples (adds ~20 ns per message; disable for raw throughput).
    pub measure_latency: bool,
    /// Shared stop signal — set to true to gracefully terminate all instances.
    pub stop: Arc<AtomicBool>,
}

// ---------------------------------------------------------------------------
// Spawn a pipeline instance on its own thread
// ---------------------------------------------------------------------------

pub fn spawn_instance(
    config: InstanceConfig,
    metrics: Arc<InstanceMetrics>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // ── 1. Pin OS thread to its assigned core ─────────────────────────
        if let Some(cores) = core_affinity::get_core_ids() {
            if !cores.is_empty() {
                let idx = config.core_id % cores.len();
                let _ = core_affinity::set_for_current(cores[idx]);
            }
        }

        // ── 2. current_thread Tokio runtime ───────────────────────────────
        //      Exact mirror of the engine's RuntimePipeline::run():
        //        Builder::new_current_thread().enable_all().build()
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build per-core tokio runtime");

        // ── 3. LocalSet — executes !Send futures on this thread ───────────
        //      The engine uses LocalSet::new() and spawn_local() for every node.
        let local = LocalSet::new();

        // ── 4. Build channel chain ─────────────────────────────────────────
        //
        //    Generator  ──[ch0]──▶  Proc0  ──[ch1]──▶ … ──[chN]──▶  Sink
        //
        //    N channels total: one per stage boundary (processors + 1)
        let n_channels = config.num_processors + 1;
        let cap = config.channel_capacity;

        // Create all channels first, then distribute ownership.
        // mpsc::Receiver<T> is !Clone so we must own them exactly once.
        let (senders, receivers): (Vec<mpsc::Sender<Msg>>, Vec<mpsc::Receiver<Msg>>) = (0..n_channels)
            .map(|_| mpsc::Channel::<Msg>::new(cap))
            .unzip();

        // senders[0]    → generator writes here
        // receivers[i]  ← proc[i] reads from here, writes to senders[i+1]
        // receivers[last] ← sink reads from here

        let mut senders = senders;   // need mutability to drain
        let mut receivers = receivers;

        // Generator takes senders[0]
        let gen_tx = senders.remove(0);
        // Processors take (receivers[i], senders[i+1]) for i in 0..num_processors
        // Sink takes the last receiver
        // After remove(0) from senders, senders now holds [ch1_tx, ch2_tx, …]
        // receivers holds [ch0_rx, ch1_rx, …]

        let payload = Bytes::from(vec![0xAB_u8; config.msg_size_bytes]);

        // ── 5. Generator task ─────────────────────────────────────────────
        local.spawn_local({
            let stop = config.stop.clone();
            let metrics = metrics.clone();
            let target_rate = config.target_rate;
            let payload = payload.clone();
            async move {
                generator(gen_tx, stop, metrics, payload, target_rate).await;
            }
        });

        // ── 6. Passthrough (processor) tasks ──────────────────────────────
        for _i in 0..config.num_processors {
            // receivers[i] is ch_i's rx; senders[i] is ch_{i+1}'s tx
            let rx = receivers.remove(0);
            let tx = senders.remove(0);
            local.spawn_local(async move {
                passthrough(rx, tx).await;
            });
        }

        // ── 7. Sink task ──────────────────────────────────────────────────
        // The last remaining receiver feeds the sink.
        let sink_rx = receivers.remove(0);
        let measure_latency = config.measure_latency;
        let metrics_sink = metrics.clone();
        local.spawn_local(async move {
            sink(sink_rx, metrics_sink, measure_latency).await;
        });

        // ── 8. Drive until stop ───────────────────────────────────────────
        rt.block_on(local);
    })
}

// ---------------------------------------------------------------------------
// Task bodies — all !Send, running inside the LocalSet
// ---------------------------------------------------------------------------

/// Generator: emits messages at target_rate msgs/sec (or as fast as possible).
async fn generator(
    tx: mpsc::Sender<Msg>,
    stop: Arc<AtomicBool>,
    metrics: Arc<InstanceMetrics>,
    payload: Bytes,
    target_rate: Option<NonZeroU32>,
) {
    // Build an optional token-bucket rate limiter.
    // governor::RateLimiter::direct() is !Send, which is fine inside LocalSet.
    let limiter = target_rate.map(|rate| {
        RateLimiter::direct(Quota::per_second(rate))
    });

    // Batch counter: yield to the executor every N successful sends so downstream
    // tasks (passthrough, sink) get scheduled without adding a context-switch on
    // every message.  When a rate limiter is active its until_ready().await already
    // provides natural yield points, so we use a larger batch there.
    let yield_every: u32 = if target_rate.is_some() { 256 } else { 16 };
    let mut send_count: u32 = 0;

    while !stop.load(Ordering::Relaxed) {
        // Wait for a rate-limiting token (noop if no limit configured).
        if let Some(ref lim) = limiter {
            lim.until_ready().await;
        }

        let ts = Instant::now();
        // send() is non-blocking: returns Err(Full) or Err(Closed) immediately.
        match tx.send((payload.clone(), ts)) {
            Ok(()) => {
                metrics.generated.fetch_add(1, Ordering::Relaxed);
                send_count = send_count.wrapping_add(1);
                if send_count % yield_every == 0 {
                    tokio::task::yield_now().await;
                }
            }
            Err(_full_or_closed) => {
                // Channel full → backpressure: record drop and yield without burning CPU.
                metrics.dropped.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
            }
        }
    }
    // Dropping tx closes the channel, unblocking all downstream tasks.
}

/// Passthrough: simulates a no-op processor node that just forwards data.
/// Measures how many stages add to end-to-end latency.
async fn passthrough(rx: mpsc::Receiver<Msg>, tx: mpsc::Sender<Msg>) {
    loop {
        match rx.recv().await {
            Ok(msg) => {
                // send_async: awaits until there is space (backpressure)
                if tx.send_async(msg).await.is_err() {
                    break; // downstream channel closed
                }
            }
            Err(_) => break, // upstream generator stopped
        }
    }
}

/// Sink: counts received messages and optionally records latency.
async fn sink(rx: mpsc::Receiver<Msg>, metrics: Arc<InstanceMetrics>, measure_latency: bool) {
    loop {
        match rx.recv().await {
            Ok((_data, ts)) => {
                metrics.received.fetch_add(1, Ordering::Relaxed);
                if measure_latency {
                    let ns = ts.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                    metrics.record_latency(ns);
                }
            }
            Err(_) => break, // generator stopped → done
        }
    }
}
