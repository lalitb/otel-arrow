// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Engine-side adapters for native cdylib plugins (phase 1).
//!
//! - [`NativeProcessorAdapter`] implements
//!   `otap_df_engine::local::processor::Processor<OtapPdata>` and routes
//!   each `Message::PData` through the plugin's `process` export.
//! - [`build_native_registry`] turns a slice of `LoadedNativePlugin`
//!   into a `DynamicComponentRegistry<OtapPdata>` consumable by the
//!   engine's static-first overlay.
//!
//! The adapter never serializes or copies the payload bytes. On
//! `ForwardSame` it forwards the original `OtapPdata` message; on
//! `Drop` it returns without emitting; on `Error` it surfaces a
//! `Error::ProcessorError` to the engine.

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod adapter;
pub mod registry_bridge;

pub use adapter::NativeProcessorAdapter;
pub use registry_bridge::{build_native_registry, extend_native_registry};
