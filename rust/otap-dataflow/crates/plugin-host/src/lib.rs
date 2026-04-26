// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Plugin host: discovery, verification, descriptor loading, validation,
//! and Wasmtime-backed runtime execution for dynamic plugins.
//!
//! Responsibilities (per RFC §5 / §8 / §14):
//!   * discover plugin manifests in configured directories
//!   * verify artifact SHA-256 (always) and signature (per policy)
//!   * compute a stable cache key for the precompiled Wasm component
//!   * call the plugin's `descriptor`, `validate-config`, and `process`
//!     exports through the [`wasmtime-backend`](crate) feature
//!   * surface a `Vec<LoadedPlugin>` (with [`PluginConfigValidator`]s and
//!     [`PluginRunner`]s) that downstream crates (`otap-df-plugin-nodes`)
//!     translate into a `DynamicComponentRegistry<PData>`.
//!
//! Wasmtime is feature-gated behind `wasmtime-backend`. Without the
//! feature, descriptor/validate/process all return
//! [`PluginError::BackendUnimplemented`] so the surface is concrete and
//! the behaviour is honest.

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod cache;
pub mod descriptor_loader;
pub mod host;
pub mod runner;
pub mod validator;

#[cfg(feature = "wasmtime-backend")]
mod wasmtime_backend;

pub use cache::{ComponentCacheKey, target_triple};
pub use descriptor_loader::load_descriptor;
pub use host::{DEFAULT_EXPORTER_BLOCKING_CONCURRENCY, LoadedPlugin, PluginHost, PluginHostConfig};
pub use runner::{PayloadKind, PluginRunner, UnimplementedRunner, result_class};
pub use validator::{PluginConfigValidator, UnimplementedValidator};
