// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Phase-1 plugin-backed adapter nodes for `OtapPdata`.
//!
//! This crate sits between [`otap_df_plugin_host`] (which loads manifests
//! and verifies artifacts) and [`otap_df_engine::runtime_registry`] (which
//! the controller validates against).
//!
//! What lives here:
//!   * [`WasmProcessorAdapter`] / [`WasmExporterAdapter`] skeletons — the
//!     types that will eventually implement
//!     `otap_df_engine::processor::Processor<OtapPdata>` /
//!     `otap_df_engine::exporter::Exporter<OtapPdata>`.
//!   * [`build_dynamic_registry`] — turns a `Vec<LoadedPlugin>` into a
//!     `DynamicComponentRegistry<OtapPdata>` with dynamic validators.
//!
//! Phase 1: the `factory` field on each registry entry is populated with
//! a real Wasmtime-backed factory closure (see [`factory`]). Pipelines
//! referencing plugin URNs can now be built and run; runtime adapters
//! convert OTLP-proto-bytes into plugin calls and back.

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod adapter;
pub mod factory;
pub mod registry_bridge;

pub use adapter::{WasmExporterAdapter, WasmProcessorAdapter};
pub use registry_bridge::build_dynamic_registry;
