// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Phase-1 plugin API surface (types only).
//!
//! Implements the host/plugin shared shapes described in
//! `docs/dynamic-processor-exporter-plugins-phase1.md`. This crate has no
//! runtime dependencies on the engine or wasmtime — it only declares the
//! types crossed between manifest parsing, host loading, descriptor
//! exchange, and the engine-side overlay registry.
//!
//! Phase 1 scope:
//!   * processor and exporter components only
//!   * payload variant supports `OtlpProtoBytes` and *reserves*
//!     `OtapArrowIpc`; the host implements only the former
//!   * plugin descriptor is authoritative for component declarations and
//!     config schema
//!   * `PluginFingerprint` is the rollout-identity carrier for live reconfig

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod descriptor;
pub mod error;
pub mod fingerprint;
pub mod payload;
pub mod version;

pub use descriptor::{ComponentDescriptor, ComponentKind, OutputArity, PluginDescriptor};
pub use error::{PluginError, PluginResultClass};
pub use fingerprint::PluginFingerprint;
pub use payload::{PayloadFormat, SignalType};
pub use version::{PLUGIN_API_VERSION, PluginApiVersion};
