// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Error types for the MCP server.

/// Errors that can occur in the MCP server.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Failed to parse a configuration string.
    #[error("configuration parse error: {details}")]
    ConfigParseError {
        /// Details of the parse error.
        details: String,
    },

    /// Failed to validate a configuration.
    #[error("configuration validation error: {details}")]
    ConfigValidationError {
        /// Details of the validation error.
        details: String,
    },

    /// Failed to connect to the admin API.
    #[error("admin API connection error: {details}")]
    AdminConnectionError {
        /// Details of the connection error.
        details: String,
    },

    /// The admin API returned an error response.
    #[error("admin API error ({status}): {details}")]
    AdminApiError {
        /// HTTP status code.
        status: u16,
        /// Details of the error.
        details: String,
    },

    /// An unknown component type was requested.
    #[error("unknown component type: {component_type}")]
    UnknownComponentType {
        /// The unrecognized component type.
        component_type: String,
    },

    /// An unknown component name was requested.
    #[error("unknown component: {name}")]
    UnknownComponent {
        /// The unrecognized component name.
        name: String,
    },

    /// An example config was not found.
    #[error("example config not found: {name}")]
    ExampleNotFound {
        /// The requested example name.
        name: String,
    },

    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
