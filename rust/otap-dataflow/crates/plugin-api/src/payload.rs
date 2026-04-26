// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Plugin payload format declarations.
//!
//! Phase 1 ABI reserves both `OtlpProtoBytes` and `OtapArrowIpc` so the WIT
//! variant is forward-compatible. The host *implements* only OTLP bytes for
//! v1; a plugin declaring only `OtapArrowIpc` is rejected by the loader.

use serde::{Deserialize, Serialize};

/// Telemetry signal carried in a payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalType {
    /// Logs.
    Logs,
    /// Metrics.
    Metrics,
    /// Traces.
    Traces,
}

/// Wire formats a plugin can declare support for.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PayloadFormat {
    /// OTLP protobuf bytes — the only format the phase-1 host accepts.
    OtlpProtoBytes,
    /// Arrow IPC stream — reserved in the ABI; not implemented in phase 1.
    /// Plugins declaring only this format are rejected at load time.
    OtapArrowIpc,
}
