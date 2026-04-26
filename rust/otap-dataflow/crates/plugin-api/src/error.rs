// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Plugin error/result classification used by the host adapter to map
//! plugin outcomes deterministically into engine ack semantics.

use serde::{Deserialize, Serialize};

/// Coarse-grained class of a plugin call result.
///
/// Adapter mapping (per RFC §11):
///   * `Success`     -> exporter ACK / processor success
///   * `Retryable`   -> retryable failure / NACK path
///   * `Permanent`   -> permanent drop, no retry
///   * `Fatal`       -> node/runtime failure, may fail candidate rollout
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginResultClass {
    /// Plugin reported success.
    Success,
    /// Plugin reported a transient failure; retry per node policy.
    Retryable,
    /// Plugin reported a non-retryable failure.
    Permanent,
    /// Plugin reported an unrecoverable error; treat as instance failure.
    Fatal,
}

/// Error returned by host loading and adapter calls.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// The plugin's declared API version is not supported.
    #[error("incompatible plugin API version: host={host}, plugin={plugin}")]
    IncompatibleApiVersion {
        /// Host plugin-API version.
        host: String,
        /// Plugin-declared plugin-API version.
        plugin: String,
    },

    /// The plugin declared only `OtapArrowIpc`, which is not supported in
    /// phase 1.
    #[error("plugin requires unsupported payload format: {format}")]
    UnsupportedPayloadFormat {
        /// Format name (e.g. "otap-arrow-ipc").
        format: String,
    },

    /// Two plugin entries declare the same component URN.
    #[error("duplicate component URN: {0}")]
    DuplicateComponentUrn(String),

    /// Manifest declared a SHA-256 that does not match the artifact bytes.
    #[error("artifact integrity check failed: {details}")]
    ArtifactIntegrity {
        /// Description of the mismatch.
        details: String,
    },

    /// Manifest required a signature that could not be verified.
    #[error("signature verification failed: {details}")]
    SignatureVerification {
        /// Description of the failure.
        details: String,
    },

    /// IO failure reading manifest, artifact, or cache directory.
    #[error("plugin IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Manifest could not be deserialized.
    #[error("manifest parse error: {0}")]
    ManifestParse(String),

    /// The plugin's runtime call (host -> plugin) failed before a result
    /// class could be computed.
    #[error("plugin runtime error: {0}")]
    Runtime(String),

    /// The Wasmtime backend feature is not enabled in this build.
    ///
    /// Returned by host APIs when `crates/plugin-host` is built without
    /// the `wasmtime-backend` Cargo feature, so a misconfiguration
    /// surfaces explicitly instead of silently succeeding.
    #[error("wasmtime backend is not available in this build: {0}")]
    BackendUnimplemented(&'static str),
}
