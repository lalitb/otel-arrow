// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Typed backend errors.

use std::path::PathBuf;

/// Categories used when an artifact no longer matches its pinned contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompatibilityErrorKind {
    /// The artifact digest differs from the pinned digest.
    ArtifactHash,
    /// The artifact architecture differs from the expected architecture.
    Architecture,
    /// An expected program is absent.
    MissingProgram,
    /// A named program changed section, type, or instruction count.
    ChangedProgramAbi,
    /// An expected map is absent.
    MissingMap,
    /// A map exists but its ABI changed.
    ChangedMapAbi,
    /// An expected global variable is absent.
    MissingGlobal,
    /// A global variable exists but its size or offset changed.
    ChangedGlobalAbi,
    /// A required tail-call destination is absent or changed.
    TailCallTopology,
    /// The compatibility-version label differs.
    CompatibilityVersion,
    /// The manifest schema or upstream revision is not supported by this backend.
    ManifestVersion,
}

/// A deterministic compatibility mismatch.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{kind:?}: {detail}")]
pub struct CompatibilityError {
    /// Stable mismatch category suitable for metrics and tests.
    pub kind: CompatibilityErrorKind,
    /// Bounded human-readable detail.
    pub detail: String,
}

impl CompatibilityError {
    /// Creates a compatibility error and truncates its detail to a fixed bound.
    pub fn new(kind: CompatibilityErrorKind, detail: impl Into<String>) -> Self {
        const MAX_DETAIL_BYTES: usize = 512;
        let mut detail = detail.into();
        truncate_detail(&mut detail, MAX_DETAIL_BYTES);
        Self { kind, detail }
    }
}

/// Configuration validation failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConfigError {
    /// A required path was empty or relative.
    #[error("{field} must be an absolute non-empty path: {path:?}")]
    InvalidPath {
        /// Configuration field name.
        field: &'static str,
        /// Rejected path.
        path: PathBuf,
    },
    /// A numeric setting was outside its supported range.
    #[error("{field} must be in {minimum}..={maximum}, got {actual}")]
    OutOfRange {
        /// Configuration field name.
        field: &'static str,
        /// Inclusive lower bound.
        minimum: u64,
        /// Inclusive upper bound.
        maximum: u64,
        /// Rejected value.
        actual: u64,
    },
    /// A collection setting was empty or contained duplicate values.
    #[error("{field}: {detail}")]
    InvalidCollection {
        /// Configuration field name.
        field: &'static str,
        /// Explanation of the invalid collection.
        detail: &'static str,
    },
    /// A digest or version field was malformed.
    #[error("{field}: {detail}")]
    InvalidText {
        /// Configuration field name.
        field: &'static str,
        /// Explanation of the malformed text.
        detail: &'static str,
    },
    /// A resource-limit relationship was inconsistent.
    #[error("resource limit {field}: {detail}")]
    InvalidLimit {
        /// Limit field name.
        field: &'static str,
        /// Explanation of the invalid relationship.
        detail: &'static str,
    },
}

/// Failures produced by the backend.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// Configuration did not pass non-privileged validation.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Artifact compatibility validation failed.
    #[error(transparent)]
    Compatibility(#[from] CompatibilityError),
    /// The pinned object's native probe bindings could not be prepared safely.
    #[error(transparent)]
    Preparation(#[from] crate::preparation::PreparationError),
    /// Filesystem input could not be read.
    #[error("I/O at {path:?}: {source}")]
    Io {
        /// Path being accessed.
        path: PathBuf,
        /// Underlying operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// Input exceeded its byte limit, regardless of its reported file size.
    #[error("input at {path:?} exceeds {maximum} bytes")]
    InputTooLarge {
        /// Input path.
        path: PathBuf,
        /// Maximum retained bytes.
        maximum: usize,
    },
    /// A PID changed generation or executable while metadata was being read.
    #[error("process {pid} changed while reading its metadata")]
    ProcessChanged {
        /// Process ID involved.
        pid: u32,
    },
    /// ELF or BTF input was malformed or unsupported.
    #[error("artifact parse failed: {0}")]
    ArtifactParse(String),
    /// A kernel-facing operation was requested on an unsupported platform.
    #[error("native eBPF operation is unsupported: {0}")]
    Unsupported(String),
    /// A bounded structure rejected input.
    #[error("capacity reached: {0}")]
    Capacity(&'static str),
    /// Kernel loading or attachment failed.
    #[error("kernel operation {operation} failed: {detail}")]
    Kernel {
        /// Stable operation label.
        operation: &'static str,
        /// Bounded diagnostic detail.
        detail: String,
    },
    /// Process metadata could not be synchronized.
    #[error("process {pid} synchronization failed: {detail}")]
    Process {
        /// Process ID involved.
        pid: u32,
        /// Bounded diagnostic detail.
        detail: String,
    },
}

impl BackendError {
    /// Creates a bounded kernel diagnostic.
    pub fn kernel(operation: &'static str, detail: impl Into<String>) -> Self {
        Self::Kernel {
            operation,
            detail: bounded_detail(detail.into()),
        }
    }

    /// Creates a bounded process diagnostic.
    pub fn process(pid: u32, detail: impl Into<String>) -> Self {
        Self::Process {
            pid,
            detail: bounded_detail(detail.into()),
        }
    }
}

fn bounded_detail(mut detail: String) -> String {
    const MAX_DETAIL_BYTES: usize = 16 * 1024;
    truncate_detail(&mut detail, MAX_DETAIL_BYTES);
    detail
}

pub(crate) fn truncate_detail(detail: &mut String, maximum: usize) {
    let mut end = maximum.min(detail.len());
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail.truncate(end);
    detail.shrink_to_fit();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: A diagnostic contains multibyte characters across either byte limit.
    /// Guarantees: Error construction never panics and retained strings stay within the limit.
    #[test]
    fn unicode_diagnostics_are_bounded() {
        let text = "\u{20ac}".repeat(10_000);
        let error = CompatibilityError::new(CompatibilityErrorKind::ArtifactHash, text.clone());
        assert_eq!(error.detail.len(), 510);
        assert!(error.detail.capacity() <= 512);
        let BackendError::Kernel { detail, .. } = BackendError::kernel("test", text) else {
            panic!("expected kernel error");
        };
        assert!(detail.len() <= 16 * 1024);
    }
}
