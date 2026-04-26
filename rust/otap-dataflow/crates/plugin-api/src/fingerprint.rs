// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Plugin artifact fingerprint participating in live-reconfig identity.

use serde::{Deserialize, Serialize};

use crate::version::PluginApiVersion;

/// Stable identity of a plugin-backed component, included in the runtime
/// shape that live reconfiguration compares.
///
/// A change to any field MUST be treated as a pipeline change so that the
/// rollout planner replaces the affected plugin instance. See RFC §6.6 and
/// §9.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct PluginFingerprint {
    /// Component URN this fingerprint is tied to.
    pub component_urn: String,
    /// Plugin manifest version (e.g. `0.1.0`).
    pub plugin_version: String,
    /// Hex-encoded SHA-256 of the plugin artifact bytes.
    pub artifact_sha256: String,
    /// Plugin API version the artifact was built against.
    pub plugin_api_version: PluginApiVersion,
}
