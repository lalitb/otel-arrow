// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Aya-based Linux loader and bounded native-only capture loop.
//!
//! This API is synchronous and intentionally not integrated into a DFE runtime.
//! It owns all kernel resources and drops them on startup failure or shutdown.

mod clock;
mod loader;
mod maps;
mod process_sync;
#[allow(unsafe_code)]
mod syscall;
mod system_analysis;

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use aya::maps::perf::PerfEvent as PerfBufferEvent;

use crate::aggregate::BoundedAggregator;
use crate::artifact::{ArtifactArchitecture, CompatibilityValidator, UpstreamArtifact};
use crate::config::BackendConfig;
use crate::error::{BackendError, CompatibilityError, CompatibilityErrorKind};
use crate::event_reader::BoundedEventReader;
use crate::inventory::CompatibilityManifest;
use crate::lifecycle::SingletonLease;
use crate::limits::MemoryEstimate;
use crate::linux::SystemProbe;
use crate::snapshot::{PendingSnapshots, ProfileSnapshot};
use crate::statistics::DropReason;

use self::loader::LoadedRuntime;
use self::process_sync::ProcessSynchronizer;

/// Fully owned native-only profiler session.
pub struct NativeSession {
    config: BackendConfig,
    runtime: Option<LoadedRuntime>,
    synchronizer: ProcessSynchronizer,
    reader: BoundedEventReader,
    aggregator: BoundedAggregator,
    pending: PendingSnapshots,
    lease: SingletonLease,
    probe: SystemProbe,
    memory_estimate: MemoryEstimate,
    stack_layout: crate::linux::system_config::KernelStackLayout,
    last_ring_output_failures: u64,
    next_process_cleanup: Instant,
    perf_cursor: usize,
}

impl NativeSession {
    /// Validates, loads, attaches, and reports readiness only after one CPU succeeds.
    pub fn start(
        config: BackendConfig,
        manifest: CompatibilityManifest,
    ) -> Result<Self, BackendError> {
        Self::start_with_hook(config, manifest, |_| Ok(()))
    }

    fn start_with_hook(
        config: BackendConfig,
        manifest: CompatibilityManifest,
        after_stage: impl FnMut(loader::StartupStage) -> Result<(), BackendError>,
    ) -> Result<Self, BackendError> {
        config.validate()?;
        let artifact = UpstreamArtifact::open(&config.object_path)?;
        if config.expected_artifact_sha256 != artifact.inventory().sha256 {
            return Err(CompatibilityError::new(
                CompatibilityErrorKind::ArtifactHash,
                format!(
                    "configuration expected {}, discovered {}",
                    config.expected_artifact_sha256,
                    artifact.inventory().sha256
                ),
            )
            .into());
        }
        let validator = CompatibilityValidator::new(manifest)?;
        validator.validate(&artifact, &config.expected_compatibility_version)?;
        let probe = SystemProbe::collect(&config.procfs_root, &config.kernel_btf_path)?;
        if !probe.capabilities.can_attempt_native_load() {
            return Err(BackendError::Unsupported(format!(
                "effective capabilities {:#x} lack CAP_BPF+CAP_PERFMON or CAP_SYS_ADMIN",
                probe.capabilities.effective
            )));
        }
        if probe.kernel.version.major < 5
            || (probe.kernel.version.major == 5 && probe.kernel.version.minor < 17)
        {
            return Err(BackendError::Unsupported(
                "the unmodified artifact contains bpf_find_vma calls and requires Linux 5.17+"
                    .to_owned(),
            ));
        }
        match (artifact.inventory().architecture, std::env::consts::ARCH) {
            (ArtifactArchitecture::Amd64, "x86_64") | (ArtifactArchitecture::Arm64, "aarch64") => {}
            (architecture, host) => {
                return Err(CompatibilityError::new(
                    CompatibilityErrorKind::Architecture,
                    format!("artifact {architecture:?} cannot run on host {host}"),
                )
                .into());
            }
        }

        let memory_estimate = MemoryEstimate::for_config(&config, probe.topology.cpu_slots());
        let clock = clock::KernelClock::collect()?;
        let lease = SingletonLease::acquire()?;
        let runtime = loader::load(&config, &artifact, &probe, after_stage)?;
        let mut aggregator = BoundedAggregator::new(config.limits.clone());
        if runtime.cpu_attachment_failures != 0 {
            aggregator.add_loss(DropReason::CpuAttachment, runtime.cpu_attachment_failures);
        }
        Ok(Self {
            synchronizer: ProcessSynchronizer::new(
                config.procfs_root.clone(),
                config.limits.clone(),
                runtime.page_size,
                clock,
            ),
            reader: BoundedEventReader::new(
                config.limits.max_queued_raw_events,
                config.limits.max_frames_per_trace,
            ),
            pending: PendingSnapshots::new(config.limits.max_pending_snapshots),
            aggregator,
            stack_layout: runtime.stack_layout,
            runtime: Some(runtime),
            config,
            lease,
            probe,
            memory_estimate,
            last_ring_output_failures: 0,
            next_process_cleanup: Instant::now() + Duration::from_secs(1),
            perf_cursor: 0,
        })
    }

    /// Returns non-privileged host evidence captured before load.
    #[must_use]
    pub const fn system_probe(&self) -> &SystemProbe {
        &self.probe
    }

    /// Returns the configured conservative memory maximum.
    #[must_use]
    pub const fn memory_estimate(&self) -> MemoryEstimate {
        self.memory_estimate
    }

    /// Returns discovered layout offsets without exposing kernel addresses.
    #[must_use]
    pub const fn kernel_stack_layout(&self) -> crate::linux::system_config::KernelStackLayout {
        self.stack_layout
    }

    /// Returns CPUs that successfully reached the attachment phase.
    #[must_use]
    pub fn selected_cpus(&self) -> &[u32] {
        self.runtime
            .as_ref()
            .map_or(&[], |runtime| runtime.selected_cpus.as_slice())
    }

    /// Returns bounded process synchronization diagnostics.
    #[must_use]
    pub fn error_details(&self) -> &[String] {
        self.synchronizer.errors().details()
    }

    /// Runs the synchronous bounded event loop and returns the oldest completed window.
    pub fn capture_for(&mut self, duration: Duration) -> Result<ProfileSnapshot, BackendError> {
        if self
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.bpf.as_ref())
            .is_none()
        {
            return Err(BackendError::Unsupported(
                "capture requested after shutdown".to_owned(),
            ));
        }
        let start = Instant::now();
        let deadline = start.checked_add(duration).ok_or_else(|| {
            BackendError::Unsupported(
                "capture duration exceeds the host monotonic clock range".to_owned(),
            )
        })?;
        let mut next_report = start
            .checked_add(self.config.reporting_interval)
            .unwrap_or(deadline);

        loop {
            if let Err(error) = self.drain_once(Some(deadline)) {
                drop(self.shutdown(Duration::ZERO));
                return Err(error);
            }
            let now = Instant::now();
            if now >= next_report && now < deadline {
                let snapshot = self.aggregator.finalize();
                let _queued = self.pending.try_push(snapshot);
                next_report = next_report
                    .checked_add(self.config.reporting_interval)
                    .unwrap_or(deadline);
            }
            if now >= deadline {
                break;
            }
            if self.reader.is_empty() {
                let cpu_count = self
                    .runtime
                    .as_ref()
                    .map_or(0, |runtime| runtime.selected_cpus.len());
                let delay = event_poll_delay(&self.config, cpu_count)
                    .min(deadline - now)
                    .min(next_report.saturating_duration_since(now))
                    .min(self.next_process_cleanup.saturating_duration_since(now));
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
            }
        }
        if let Err(error) = self.drain_once(Some(deadline)) {
            drop(self.shutdown(Duration::ZERO));
            return Err(error);
        }
        let final_snapshot = self.aggregator.finalize();
        let rejected = self.pending.try_push(final_snapshot).err();
        let handoff_losses = self.pending.take_losses().non_zero();
        if let Some(mut snapshot) = self.pending.pop() {
            merge_snapshot_losses(&mut snapshot, handoff_losses);
            Ok(snapshot)
        } else if let Some(snapshot) = rejected {
            Ok(snapshot)
        } else {
            Err(BackendError::Unsupported(
                "capture produced no reporting generation".to_owned(),
            ))
        }
    }

    /// Disables sampling, drains until the deadline, finalizes, and releases all resources.
    ///
    /// Repeated calls are harmless. The deadline bounds optional draining and
    /// finalization; mandatory resource release is not a real-time OS guarantee.
    /// A zero deadline skips optional work and performs cleanup.
    pub fn shutdown(&mut self, deadline: Duration) -> Result<Vec<ProfileSnapshot>, BackendError> {
        let Some(runtime) = self.runtime.as_mut() else {
            return Ok(Vec::new());
        };
        let end = Instant::now().checked_add(deadline);
        let bpf = runtime.bpf.take();
        drop(bpf);
        let mut drained = false;
        let mut failure = if end.is_none() {
            Some(BackendError::Unsupported(
                "shutdown duration exceeds the host clock range".to_owned(),
            ))
        } else {
            None
        };
        while end.is_some_and(|end| Instant::now() < end) {
            match self.drain_once(end) {
                Ok(0) => {
                    drained = end.is_some_and(|end| Instant::now() < end);
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    self.synchronizer.record_error(&error);
                    failure = Some(error);
                    break;
                }
            }
        }
        self.reader.discard(DropReason::ShutdownDeadline);
        for (reason, count) in self.reader.take_losses().non_zero() {
            self.aggregator.add_loss(reason, count);
        }
        for (reason, count) in self.synchronizer.take_losses().non_zero() {
            self.aggregator.add_loss(reason, count);
        }
        let mut snapshot = self
            .aggregator
            .finalize_until(end.unwrap_or_else(Instant::now));
        snapshot.statistics.shutdown_incomplete |= !drained;
        merge_snapshot_losses(&mut snapshot, self.pending.take_losses().non_zero());
        let mut snapshots = Vec::with_capacity(self.pending.len() + 1);
        while let Some(pending) = self.pending.pop() {
            snapshots.push(pending);
        }
        snapshots.push(snapshot);
        drop(self.runtime.take());
        self.synchronizer.release_userspace();
        self.reader = BoundedEventReader::new(0, 0);
        self.pending = PendingSnapshots::new(0);
        self.lease.release();
        match failure {
            Some(error) => Err(error),
            None => Ok(snapshots),
        }
    }

    fn drain_once(&mut self, deadline: Option<Instant>) -> Result<usize, BackendError> {
        let runtime = self.runtime.as_mut().ok_or_else(|| {
            BackendError::Unsupported("event drain requested after shutdown".to_owned())
        })?;
        let expired = || deadline.is_some_and(|end| Instant::now() >= end);
        if expired() {
            return Ok(0);
        }
        let mut perf_lost = 0_u64;
        let mut notification_losses = 0_u64;
        let mut perf_count = 0;
        let perf_budget = self.config.limits.max_raw_events_per_drain / 2;
        let buffer_count = runtime.report_buffers.len();
        for offset in 0..buffer_count {
            if perf_count == perf_budget || expired() {
                break;
            }
            let index = (self.perf_cursor + offset) % buffer_count;
            let buffer = &mut runtime.report_buffers[index];
            if !buffer.readable() {
                continue;
            }
            let budget = perf_budget - perf_count;
            let consumed = buffer.try_fold(0_usize, |count, event| {
                match event {
                    PerfBufferEvent::Sample { head, tail } => {
                        if !decode_perf_notification(head, tail) {
                            notification_losses = notification_losses.saturating_add(1);
                        }
                    }
                    PerfBufferEvent::Lost { count } => {
                        perf_lost = perf_lost.saturating_add(count);
                    }
                }
                if count + 1 >= budget || expired() {
                    ControlFlow::Break(count + 1)
                } else {
                    ControlFlow::Continue(count + 1)
                }
            });
            perf_count += match consumed {
                ControlFlow::Break(count) | ControlFlow::Continue(count) => count,
            };
        }
        if buffer_count != 0 {
            self.perf_cursor = (self.perf_cursor + 1) % buffer_count;
        }
        self.aggregator
            .add_loss(DropReason::UnknownNotification, notification_losses);
        if perf_lost != 0 {
            self.aggregator
                .add_loss(DropReason::PerfBufferLost, perf_lost);
        }
        if !expired() {
            let ring_failures = runtime
                .metrics
                .get(&105, 0)
                .map_err(|error| BackendError::kernel("read ring loss metric", error.to_string()))?
                .iter()
                .fold(0_u64, |total, value| {
                    total.saturating_add(u64::from_le_bytes(*value))
                });
            let ring_failure_delta = ring_failures.saturating_sub(self.last_ring_output_failures);
            self.last_ring_output_failures = ring_failures;
            if ring_failure_delta != 0 {
                self.aggregator
                    .add_loss(DropReason::RingBufferLost, ring_failure_delta);
            }
        }

        let mut ring_count = 0;
        while ring_count < self.config.limits.max_raw_events_per_drain - perf_count && !expired() {
            let Some(item) = runtime.ring.next() else {
                break;
            };
            let _accepted = self.reader.push_raw(&item);
            ring_count += 1;
        }

        self.synchronizer.begin_drain();
        if runtime.bpf.is_some() && !expired() && Instant::now() >= self.next_process_cleanup {
            self.synchronizer.cleanup_processes(&mut runtime.maps)?;
            self.next_process_cleanup = Instant::now() + Duration::from_secs(1);
        }
        if runtime.bpf.is_some() && !expired() {
            self.synchronizer.drain_pid_events(&mut runtime.maps)?;
        }

        let queued_before = self.reader.len();
        let traces = if let Some(end) = deadline {
            self.reader
                .drain_until(self.config.limits.max_raw_events_per_drain, end)
        } else {
            self.reader
                .drain(self.config.limits.max_raw_events_per_drain)
        };
        let queued_count = queued_before - self.reader.len();
        let mut remaining = traces.len();
        for mut trace in traces {
            if expired() {
                let reason = if runtime.bpf.is_some() {
                    DropReason::CaptureDeadline
                } else {
                    DropReason::ShutdownDeadline
                };
                self.aggregator.add_loss(reason, remaining as u64);
                break;
            }
            remaining -= 1;
            let Some(identity) = self.synchronizer.identity_for_trace(
                trace.pid,
                trace.ktime_ns,
                &mut runtime.maps,
                runtime.bpf.is_some(),
            )?
            else {
                continue;
            };
            if !self.config.include_kernel_stacks {
                trace.kernel_frames.clear();
            }
            let _accepted = self.aggregator.record(trace, identity);
        }
        for (reason, count) in self.reader.take_losses().non_zero() {
            self.aggregator.add_loss(reason, count);
        }
        for (reason, count) in self.synchronizer.take_losses().non_zero() {
            self.aggregator.add_loss(reason, count);
        }
        Ok(perf_count + ring_count + queued_count)
    }
}

impl Drop for NativeSession {
    fn drop(&mut self) {
        drop(self.shutdown(Duration::ZERO));
    }
}

fn decode_perf_notification(head: &[u8], tail: &[u8]) -> bool {
    if head.len().saturating_add(tail.len()) != 4 {
        return false;
    }
    let mut bytes = [0_u8; 4];
    let head_len = head.len().min(bytes.len());
    bytes[..head_len].copy_from_slice(&head[..head_len]);
    let remaining = bytes.len() - head_len;
    let tail_len = tail.len().min(remaining);
    bytes[head_len..head_len + tail_len].copy_from_slice(&tail[..tail_len]);
    head_len + tail_len == bytes.len()
        && matches!(
            crate::abi::decode_notification(&bytes),
            Ok(crate::abi::NotificationEvent::GenericPid)
        )
}

fn event_poll_delay(config: &BackendConfig, cpu_count: usize) -> Duration {
    // The pinned object uses BPF_RB_NO_WAKEUP, so blocking indefinitely on its
    // FD is not correct. Limit the timer to half a worst-case ring fill/drain
    // budget while avoiding a fixed 1 kHz syscall loop at low sample rates.
    let samples_per_second = u64::from(config.sampling_frequency_hz)
        .saturating_mul(cpu_count as u64)
        .max(1);
    let events = (config.limits.ring_buffer_bytes / (crate::abi::TRACE_MAX_SIZE + 8))
        .min(config.limits.max_raw_events_per_drain)
        .max(1) as u64;
    let safe_nanos = events.saturating_mul(1_000_000_000) / samples_per_second / 2;
    config
        .event_poll_interval
        .min(Duration::from_nanos(safe_nanos.max(1)))
}

fn merge_snapshot_losses(snapshot: &mut ProfileSnapshot, losses: Vec<(DropReason, u64)>) {
    for (reason, count) in losses {
        if let Some((_, existing)) = snapshot
            .statistics
            .losses
            .iter_mut()
            .find(|(candidate, _)| *candidate == reason)
        {
            *existing = existing.saturating_add(count);
        } else {
            snapshot.statistics.losses.push((reason, count));
        }
    }
    snapshot
        .statistics
        .losses
        .sort_by_key(|(reason, _)| *reason);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: A four-byte PID notification straddles any possible perf-buffer wrap position.
    /// Guarantees: Split and contiguous notifications decode identically; extra, missing, and unknown bytes are rejected.
    #[test]
    fn perf_notification_wraps_are_checked() {
        let bytes = 1_u32.to_le_bytes();
        for split in 0..=4 {
            assert!(decode_perf_notification(&bytes[..split], &bytes[split..]));
        }

        assert!(!decode_perf_notification(&bytes[..3], &[]));
        assert!(!decode_perf_notification(&bytes, &[0]));
        assert!(!decode_perf_notification(&99_u32.to_le_bytes(), &[]));
    }

    /// Scenario: Sampling ranges from one low-rate CPU to sixteen high-rate CPUs.
    /// Guarantees: Idle polling uses the configured interval, while high rates shorten it to the ring/drain budget.
    #[test]
    fn poll_interval_respects_sampling_and_buffer_capacity() {
        let mut config =
            BackendConfig::new("/artifact.o".into(), crate::Sha256Digest::ZERO, "test");
        assert_eq!(event_poll_delay(&config, 1), Duration::from_millis(10));
        config.sampling_frequency_hz = 10_000;
        assert!(event_poll_delay(&config, 16) < Duration::from_millis(2));
        assert!(!event_poll_delay(&config, 16).is_zero());
    }

    /// Scenario: An explicitly authorized runner injects failure after each real kernel startup phase.
    /// Guarantees: Partial startup releases descriptors and the singleton, and repeated full shutdown is harmless.
    #[test]
    #[ignore = "requires authorized BPF/perf capabilities, tracefs, and UPSTREAM_EBPF_OBJECT"]
    fn privileged_startup_rollback_and_shutdown() {
        use crate::config::{CpuSelection, PidNamespaceMode};
        use loader::StartupStage;

        assert_eq!(
            std::env::var("OTEL_ARROW_EBPF_PRIVILEGED_TEST").as_deref(),
            Ok("1")
        );
        let path = std::env::var_os("UPSTREAM_EBPF_OBJECT").expect("external artifact");
        let manifest = CompatibilityManifest::from_json(crate::AMD64_COMPATIBILITY_MANIFEST)
            .expect("manifest");
        let mut config = BackendConfig::new(
            path.into(),
            manifest.artifact.sha256,
            &manifest.compatibility_version,
        );
        config.cpus = CpuSelection::List(vec![0]);
        config.pid_namespace = PidNamespaceMode::Current;
        let descriptors = || {
            std::fs::read_dir("/proc/self/fd")
                .expect("descriptor directory")
                .count()
        };
        let baseline = descriptors();
        for failure_stage in [
            StartupStage::AnalysisResourcesCreated,
            StartupStage::SystemAnalysisCompleted,
            StartupStage::MapsCreated,
            StartupStage::NativeProgramsLoaded,
            StartupStage::TailCallsInitialized,
            StartupStage::ReadersCreated,
            StartupStage::LifecycleAttached,
            StartupStage::CpuAttached,
        ] {
            let result =
                NativeSession::start_with_hook(config.clone(), manifest.clone(), |stage| {
                    if stage == failure_stage {
                        Err(BackendError::kernel(
                            "injected startup failure",
                            format!("{stage:?}"),
                        ))
                    } else {
                        Ok(())
                    }
                });
            assert!(matches!(
                result,
                Err(BackendError::Kernel {
                    operation: "injected startup failure",
                    ..
                })
            ));
            assert_eq!(
                descriptors(),
                baseline,
                "descriptor leak after {failure_stage:?}"
            );
            drop(SingletonLease::acquire().expect("lease released after startup failure"));
        }
        let mut session =
            NativeSession::start(config, manifest).expect("full startup after rollback");
        let trace = crate::abi::RawTrace {
            pid: std::process::id(),
            tid: std::process::id(),
            ktime_ns: 1,
            comm: b"deadline".to_vec(),
            apm_transaction_id: [0; 8],
            apm_trace_id: [0; 16],
            custom_labels: Vec::new(),
            origin: 1,
            value: 1,
            cpu_id: 0,
            kernel_frames: Vec::new(),
            user_frames: Vec::new(),
        };
        let identity = crate::process::ProcessIdentity {
            pid: trace.pid,
            start_time_ticks: 1,
            executable: None,
        };
        session
            .aggregator
            .record(trace.clone(), identity.clone())
            .expect("first aggregate");
        session
            .aggregator
            .record(trace, identity)
            .expect("second aggregate");
        for _ in 0..3 {
            assert!(session.reader.push_raw(&[0; 8]));
        }
        let final_windows = session
            .shutdown(Duration::ZERO)
            .expect("immediate shutdown");
        assert!(!final_windows.is_empty());
        assert!(
            final_windows
                .last()
                .expect("final window")
                .statistics
                .shutdown_incomplete
        );
        let final_window = final_windows.last().expect("final window");
        assert!(final_window.samples.is_empty());
        assert_eq!(
            final_window
                .statistics
                .losses
                .iter()
                .find(|(reason, _)| *reason == DropReason::ShutdownDeadline),
            Some(&(DropReason::ShutdownDeadline, 5)),
        );
        assert_eq!(descriptors(), baseline, "descriptor leak after shutdown");
        assert!(
            session
                .shutdown(Duration::ZERO)
                .expect("repeated shutdown")
                .is_empty()
        );
        drop(SingletonLease::acquire().expect("lease released after shutdown"));
    }
}
