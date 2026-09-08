// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Typed profiler errors.

use std::path::PathBuf;

use crate::DropReason;

/// Result type used by the profiler.
pub type Result<T> = std::result::Result<T, ProfilerError>;

/// Errors returned by profiler configuration, startup, collection, and shutdown.
#[derive(Debug, thiserror::Error)]
pub enum ProfilerError {
    /// The current operating system is unsupported.
    #[error("unsupported operating system: {0}")]
    UnsupportedOperatingSystem(String),
    /// The current CPU architecture is unsupported.
    #[error("unsupported architecture: {0}")]
    UnsupportedArchitecture(String),
    /// Static configuration is invalid.
    #[error("invalid configuration for {field}: {reason}")]
    InvalidConfiguration {
        /// Configuration field name.
        field: &'static str,
        /// Stable human-readable reason.
        reason: String,
    },
    /// A profiler instance already owns the process-local singleton lease.
    #[error("a host-wide profiler is already running in this process")]
    AlreadyRunning,
    /// The process lacks required kernel capabilities.
    #[error("insufficient privileges: {0}")]
    InsufficientPrivileges(String),
    /// The running kernel is unsupported.
    #[error("unsupported kernel: {0}")]
    UnsupportedKernel(String),
    /// Required BTF data is unavailable.
    #[error("required BTF data is unavailable at {0}")]
    UnavailableBtf(PathBuf),
    /// CPU or NUMA topology discovery failed.
    #[error("topology discovery failed: {0}")]
    TopologyDiscovery(String),
    /// CPU selection did not resolve to a usable set.
    #[error("CPU selection failed: {0}")]
    CpuSelection(String),
    /// Opening a perf event failed.
    #[error("perf_event_open failed for CPU {cpu}: {reason}")]
    PerfEventOpen {
        /// Logical CPU identifier.
        cpu: u32,
        /// Underlying reason.
        reason: String,
    },
    /// The eBPF verifier rejected the program.
    #[error("eBPF verifier rejected the program: {0}")]
    VerifierRejected(String),
    /// Loading the eBPF object or program failed.
    #[error("eBPF program load failed: {0}")]
    ProgramLoad(String),
    /// Attaching the eBPF program failed.
    #[error("eBPF program attachment failed for CPU {cpu}: {reason}")]
    ProgramAttachment {
        /// Logical CPU identifier.
        cpu: u32,
        /// Underlying reason.
        reason: String,
    },
    /// Creating or opening an eBPF map failed.
    #[error("eBPF map creation failed: {0}")]
    MapCreation(String),
    /// The kernel/user event ABI is incompatible or malformed.
    #[error("kernel event ABI mismatch: {0}")]
    AbiMismatch(String),
    /// A collection worker failed to start.
    #[error("worker startup failed for shard {shard}: {reason}")]
    WorkerStartup {
        /// Shard identifier.
        shard: usize,
        /// Underlying reason.
        reason: String,
    },
    /// Startup failed after resources had been acquired.
    #[error("partial startup failed: {0}")]
    PartialStartup(String),
    /// A running collection or finalization worker failed.
    #[error("profiler worker {shard} failed: {reason}")]
    WorkerFailed {
        /// Shard identifier, or `usize::MAX` for the finalizer.
        shard: usize,
        /// Bounded diagnostic detail.
        reason: String,
    },
    /// A bounded table or byte budget cannot admit another sample.
    #[error("profiler capacity exhausted: {0:?}")]
    Capacity(DropReason),
    /// The sample predates the process generation now visible in procfs.
    #[error("sample predates process generation for PID {0}")]
    StaleProcess(u32),
    /// A requested bounded allocation failed.
    #[error("allocation failed for {0}")]
    Allocation(&'static str),
    /// Process or thread metadata lookup failed.
    #[error("process metadata lookup failed for PID {pid}: {reason}")]
    ProcessMetadata {
        /// Process identifier.
        pid: u32,
        /// Underlying reason.
        reason: String,
    },
    /// The completed-snapshot consumer disconnected.
    #[error("snapshot consumer disconnected")]
    SnapshotConsumerDisconnected,
    /// A bounded receive operation timed out.
    #[error("snapshot receive timed out")]
    SnapshotReceiveTimeout,
    /// Shutdown did not complete before the deadline.
    #[error("profiler shutdown timed out")]
    ShutdownTimeout,
    /// A required invariant was violated.
    #[error("internal profiler invariant violated: {0}")]
    InternalInvariant(String),
    /// An operating-system I/O operation failed.
    #[error("{operation} failed for {path}: {source}")]
    Io {
        /// Stable operation name.
        operation: &'static str,
        /// Affected path.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

impl ProfilerError {
    pub(crate) fn invalid(field: &'static str, reason: impl Into<String>) -> Self {
        Self::InvalidConfiguration {
            field,
            reason: reason.into(),
        }
    }
}

pub(crate) fn bounded_message(value: impl std::fmt::Display, max_bytes: usize) -> String {
    use std::fmt::Write;

    struct Bounded {
        text: String,
        limit: usize,
    }
    impl Write for Bounded {
        fn write_str(&mut self, value: &str) -> std::fmt::Result {
            let remaining = self.limit.saturating_sub(self.text.len());
            let mut end = remaining.min(value.len());
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            self.text.push_str(&value[..end]);
            if end == value.len() {
                Ok(())
            } else {
                Err(std::fmt::Error)
            }
        }
    }
    let mut output = Bounded {
        text: String::with_capacity(max_bytes),
        limit: max_bytes,
    };
    // Exhausting the fixed diagnostic budget intentionally stops formatting.
    let _bounded_result = write!(output, "{value}");
    output.text
}
