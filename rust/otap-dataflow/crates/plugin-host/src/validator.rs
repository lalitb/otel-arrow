// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Plugin-side config validator trait — wasmtime-free public surface.
//!
//! `plugin-nodes` consumes [`PluginConfigValidator`] objects produced by
//! [`crate::PluginHost`] and wraps them into the engine's
//! `ConfigValidator::Dynamic` variant. Because this trait is the only thing
//! crossing the host → engine boundary, downstream crates (notably
//! `plugin-nodes`) do not need a dependency on `wasmtime`.

/// Validate a plugin component's user config (JSON string).
///
/// Returns `Err(message)` when the plugin rejects the config; the message
/// is propagated to operators verbatim.
pub trait PluginConfigValidator: Send + Sync {
    /// Run validation against the JSON-encoded config.
    fn validate(&self, config_json: &str) -> Result<(), String>;
}

/// Phase-1 fail-closed validator. Used when the wasmtime backend is not
/// compiled in and we still want to register the plugin URN with a real
/// validator object (rather than silently accepting all configs).
pub struct UnimplementedValidator;

impl PluginConfigValidator for UnimplementedValidator {
    fn validate(&self, _config_json: &str) -> Result<(), String> {
        Err("plugin-backed component validation requires the \
             `wasmtime-backend` feature, which is not enabled in this build"
            .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unimplemented_validator_fails_closed() {
        let v = UnimplementedValidator;
        let err = v.validate("{}").unwrap_err();
        assert!(err.contains("wasmtime-backend"));
    }
}
