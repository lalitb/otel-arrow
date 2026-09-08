// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Experimental Rust userspace for the OpenTelemetry eBPF Profiler artifact.
//!
//! The crate separates non-privileged artifact compatibility checks from the
//! optional Linux kernel-loading path. Its public output is owned and
//! transport-neutral; no BPF, OTLP, Arrow, or dataflow-engine handle crosses
//! the backend boundary.

#![deny(unsafe_code)]
#![cfg_attr(not(all(feature = "native", target_os = "linux")), forbid(unsafe_code))]

pub mod abi;
pub mod aggregate;
pub mod artifact;
pub mod config;
pub mod error;
pub mod event_reader;
mod input;
pub mod inventory;
pub mod lifecycle;
pub mod limits;
pub mod linux;
pub mod mappings;
pub mod preparation;
pub mod process;
pub mod snapshot;
pub mod statistics;
pub mod unwind;

#[cfg(all(feature = "native", target_os = "linux"))]
pub mod native;

pub use aggregate::BoundedAggregator;
pub use artifact::{CompatibilityValidator, Sha256Digest, UpstreamArtifact};
pub use config::{BackendConfig, CpuSelection, PidNamespaceMode};
pub use error::{BackendError, CompatibilityError, CompatibilityErrorKind};
pub use inventory::{ArtifactInventory, CompatibilityManifest};
pub use limits::{MemoryEstimate, ResourceLimits};
pub use preparation::PreparedObject;
pub use snapshot::ProfileSnapshot;

/// The upstream revision pinned by the checked-in amd64 compatibility manifest.
pub const PINNED_UPSTREAM_COMMIT: &str = "06ea040c39d3d17bc1534a5dcc044368caf48782";

/// The checked-in compatibility manifest for the pinned amd64 artifact.
pub const AMD64_COMPATIBILITY_MANIFEST: &str =
    include_str!("../compatibility/upstream-06ea040c-amd64.json");

/// The checked-in arm64 inventory; live arm64 profiling has not been demonstrated.
pub const ARM64_COMPATIBILITY_MANIFEST: &str =
    include_str!("../compatibility/upstream-06ea040c-arm64.json");
