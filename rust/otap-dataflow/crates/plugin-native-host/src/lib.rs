// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Native cdylib plugin host for `otap-dataflow`.
//!
//! This crate is the native counterpart to [`otap_df_plugin_host`].
//! Where the latter loads Wasmtime components and exchanges OTLP-bytes
//! payloads, this crate loads native cdylibs and exchanges **opaque
//! handles** to host-owned `OtapPdata`. No payload bytes cross the FFI
//! boundary.
//!
//! # Phase 1 scope
//!
//! - Native processor plugins only (verbs `ForwardSame`, `Drop`, `Error`).
//! - Per-instance state created via the plugin's `instance_new`.
//! - Single ABI version: [`OTAP_PLUGIN_ABI_VERSION_V1`]. Plugins built
//!   against future ABIs are rejected at load time with a missing-symbol
//!   error.
//! - **Phase 1 never explicitly unloads plugins.** The library remains
//!   mapped while any loaded plugin, factory, or instance reference is
//!   alive. `df_engine` keeps loaded plugin handles alive in `main`'s
//!   scope for the lifetime of the process; live reconfig replaces the
//!   plugin *instance* but not the library mapping.
//!
//! # Trust model and isolation
//!
//! Native plugins execute with the **same trust as code compiled into
//! the collector**. There is no isolation:
//!
//! * Plugins **MUST** ensure no panic crosses the ABI boundary —
//!   either by building with `panic = "abort"` (the recommended
//!   profile, used by the sample plugin) or by wrapping every exported
//!   function body in an in-plugin `catch_unwind` and converting any
//!   caught panic to [`otap_df_plugin_abi::OtapPluginVerb::Error`].
//! * The host **does not** catch plugin panics. Plugin entry points
//!   are declared `extern "C"`; an uncaught panic crossing the
//!   boundary is undefined behavior and aborts the process under
//!   modern rustc. There is no recovery from a plugin panic.
//! * A plugin that corrupts memory, dereferences null, or invokes UB
//!   can corrupt the host or terminate the process. The host cannot
//!   recover from this.
//! * Operators are expected to require signed plugins
//!   (`require_signed: true`) in production.
//!
//! [`OTAP_PLUGIN_ABI_VERSION_V1`]: otap_df_plugin_abi::OTAP_PLUGIN_ABI_VERSION_V1
//! [`otap_df_plugin_host`]: https://docs.rs/otap-df-plugin-host

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]
#![allow(unsafe_code)]

pub mod handle;
pub mod host;
pub mod runner;

pub use handle::{HostVTableProvider, host_vtable};
pub use host::{
    LoadedNativePlugin, NativeCacheKey, NativePluginHost, NativePluginHostConfig, target_triple,
};
pub use runner::{
    NativePluginConfigValidator, NativeProcessorRunner, NativeVerb, PluginInstanceHandle,
    SharedPluginLibrary,
};
