// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Plugin-side runtime invocation trait — wasmtime-free public surface.
//!
//! [`PluginRunner`] is the runtime counterpart to
//! [`crate::PluginConfigValidator`]. `plugin-nodes` adapters
//! ([`WasmProcessorAdapter`](../../../plugin-nodes/src/adapter.rs),
//! [`WasmExporterAdapter`](../../../plugin-nodes/src/adapter.rs)) hold an
//! `Arc<dyn PluginRunner>` and call into it on each pdata message.
//!
//! Phase 1 ABI shape (raw component-model `(u32, list<u8>, string)` →
//! `result<tuple<u32, list<u8>>, string>`):
//!
//! * `signal`     — 0 = Logs, 1 = Metrics, 2 = Traces
//! * `payload`    — encoded according to [`PayloadKind`]
//! * `config_json` — the same JSON string the plugin already accepted
//!   through `validate-config`
//!
//! Returned `(class, emitted_bytes)`:
//!
//! * `class == 0` (Ok)        — emit `emitted_bytes` (or treat as no-op
//!   when empty)
//! * `class == 1` (Drop)      — filter the message, do not emit
//! * `class == 2` (Retryable) — exporter should NACK with retry semantics
//! * `class == 3` (Permanent) — exporter should NACK without retry; the
//!   adapter surfaces this as a node error
//!
//! All other classes are treated as `Permanent`. This contract is
//! intentionally minimal for phase 1; richer ack/nack semantics can be
//! layered on later without breaking the wire format.

use std::fmt::Debug;

/// Wire-level payload encoding the plugin expects.
///
/// Phase 1 only implements [`PayloadKind::OtlpProtoBytes`]. The
/// [`PayloadKind::OtapArrowIpc`] variant is reserved in the ABI to keep
/// numeric tags stable when Arrow IPC support is added; passing it today
/// produces a clear error from the adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadKind {
    /// Protobuf-encoded `ExportXxxServiceRequest` (logs / metrics / traces).
    OtlpProtoBytes,
    /// Apache Arrow IPC stream of OTAP records. Reserved; not implemented.
    OtapArrowIpc,
}

impl PayloadKind {
    /// Stable wire tag for the payload kind.
    #[must_use]
    pub fn tag(self) -> u32 {
        match self {
            PayloadKind::OtlpProtoBytes => 0,
            PayloadKind::OtapArrowIpc => 1,
        }
    }
}

/// Plugin runtime invocation trait.
///
/// Implementors must be safe to call from multiple threads (`Send + Sync`)
/// — but each call should be thought of as exclusive: phase-1
/// implementations create a fresh Wasmtime `Store` per call, so there is no
/// shared mutable plugin state across calls. If/when stores are reused for
/// performance, implementations are expected to serialize internally.
pub trait PluginRunner: Send + Sync + Debug {
    /// Invoke the plugin's `process` export.
    ///
    /// # Parameters
    /// * `signal`      — `0`=Logs, `1`=Metrics, `2`=Traces
    /// * `payload_kind` — encoding of `payload` bytes
    /// * `payload`     — opaque payload bytes
    /// * `config_json` — JSON-encoded user config (already validated)
    /// * `timeout_ms`  — host-enforced per-call deadline in milliseconds
    ///   (RFC §6.2). Implementations must return an error tagged with
    ///   ["plugin deadline exceeded"](crate::wasmtime_backend::DEADLINE_ERROR_PREFIX)
    ///   when the deadline is hit.
    ///
    /// # Returns
    /// `(class, emitted_bytes)` on success. See module docs for class
    /// codes. Implementations propagate plugin errors as `Err(string)`.
    fn process(
        &self,
        signal: u32,
        payload_kind: PayloadKind,
        payload: &[u8],
        config_json: &str,
        timeout_ms: u64,
    ) -> Result<(u32, Vec<u8>), String>;
}

/// Fail-closed runner used when no real backend is available — typically
/// because the `wasmtime-backend` feature was not compiled in. Returning
/// an error from [`PluginRunner::process`] forces the adapter to fail
/// pipeline build (factories check this proactively at instantiation
/// time, not on first message).
#[derive(Debug)]
pub struct UnimplementedRunner;

impl PluginRunner for UnimplementedRunner {
    fn process(
        &self,
        _signal: u32,
        _payload_kind: PayloadKind,
        _payload: &[u8],
        _config_json: &str,
        _timeout_ms: u64,
    ) -> Result<(u32, Vec<u8>), String> {
        Err("plugin runtime execution requires the `wasmtime-backend` \
             feature, which is not enabled in this build"
            .into())
    }
}

/// Result class codes returned by the plugin (see module docs).
pub mod result_class {
    /// Emit the returned bytes (no-op if empty).
    pub const OK: u32 = 0;
    /// Filter the message; do not emit.
    pub const DROP: u32 = 1;
    /// Retryable error.
    pub const RETRYABLE: u32 = 2;
    /// Permanent error.
    pub const PERMANENT: u32 = 3;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unimplemented_runner_fails_closed() {
        let r = UnimplementedRunner;
        let err = r
            .process(0, PayloadKind::OtlpProtoBytes, &[], "{}", 10)
            .unwrap_err();
        assert!(err.contains("wasmtime-backend"));
    }

    #[test]
    fn payload_kind_tags_are_stable() {
        assert_eq!(PayloadKind::OtlpProtoBytes.tag(), 0);
        assert_eq!(PayloadKind::OtapArrowIpc.tag(), 1);
    }
}
