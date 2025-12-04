// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Error types for the extension system.

use thiserror::Error;

/// Errors that can occur in the extension system.
#[derive(Debug, Error)]
pub enum ExtensionError {
    /// Extension not found by URN.
    #[error("extension not found: {urn}")]
    NotFound { urn: String },

    /// Extension not found by ID.
    #[error("extension not found by id: {id}")]
    NotFoundById { id: String },

    /// Extension does not implement the requested capability.
    #[error("extension '{id}' does not implement {capability}")]
    CapabilityNotSupported { id: String, capability: &'static str },

    /// Invalid extension configuration.
    #[error("invalid extension configuration: {message}")]
    InvalidConfig { message: String },

    /// Extension initialization failed.
    #[error("extension initialization failed: {message}")]
    InitializationFailed { message: String },

    /// Extension start failed.
    #[error("extension start failed: {message}")]
    StartFailed { message: String },

    /// Extension shutdown failed.
    #[error("extension shutdown failed: {message}")]
    ShutdownFailed { message: String },

    /// Generic extension error.
    #[error("extension error: {0}")]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

impl ExtensionError {
    /// Creates an `InvalidConfig` error from a serde_json error.
    pub fn from_json_error(err: serde_json::Error) -> Self {
        Self::InvalidConfig {
            message: err.to_string(),
        }
    }
}
