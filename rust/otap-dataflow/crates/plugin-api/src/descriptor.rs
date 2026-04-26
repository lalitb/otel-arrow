// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Plugin descriptor types — the host queries a plugin's `descriptor()`
//! export to learn which component URNs it provides, which payload formats
//! it supports, and (optionally) JSON Schema for each component config.
//!
//! Phase 1 makes the descriptor authoritative for component declarations.
//! Manifests are *not* a source of truth for component lists or schemas.

use serde::{Deserialize, Serialize};

use crate::payload::PayloadFormat;
use crate::version::PluginApiVersion;

/// Kind of component a descriptor entry declares.
///
/// Phase 1 only supports `Processor` and `Exporter`. Receivers and
/// extensions are documented in the enum so the type doesn't need to break
/// when phase 2 adds them, but the host rejects them at load time.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    /// Processor node (phase 1: single/default-output only).
    Processor,
    /// Exporter node.
    Exporter,
    /// Receiver node — not supported in phase 1; reserved.
    Receiver,
    /// Extension — not supported in phase 1; reserved.
    Extension,
}

/// Output arity for a processor component.
///
/// Phase 1 RFC restricts plugin processors to single/default-output. The
/// descriptor declares its arity explicitly so the host can reject plugins
/// that would require fan-out wiring before the engine adapter supports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputArity {
    /// Single default output (phase-1 supported shape).
    #[default]
    Single,
    /// Multiple outputs (e.g. routing/splitting). Reserved for phase 2.
    Multi,
}

/// One component a plugin provides.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComponentDescriptor {
    /// Canonical component URN (e.g. `urn:acme:processor:redact`).
    pub urn: String,
    /// Component kind.
    pub kind: ComponentKind,
    /// Wire formats this component supports. Phase 1 requires this list to
    /// include `OtlpProtoBytes`; otherwise the host rejects the plugin.
    pub supported_payloads: Vec<PayloadFormat>,
    /// Output arity for processors. Ignored for non-processor kinds.
    /// Defaults to [`OutputArity::Single`] for backward-compatible
    /// descriptors that omit the field.
    #[serde(default)]
    pub output_arity: OutputArity,
    /// Optional JSON Schema string for the node user-config.
    ///
    /// `validate_config` remains the authoritative validator; the schema is
    /// for documentation and admin-UI tooling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema_json: Option<String>,
}

/// A plugin descriptor — the result of calling `descriptor()` on a plugin.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginDescriptor {
    /// Plugin-declared name (matches manifest `metadata.name` for sanity).
    pub name: String,
    /// Plugin-declared version (matches manifest `metadata.version`).
    pub version: String,
    /// Plugin API version this plugin was built against.
    pub plugin_api_version: PluginApiVersion,
    /// Components contributed by this plugin.
    pub components: Vec<ComponentDescriptor>,
}

impl PluginDescriptor {
    /// Returns true if the descriptor includes at least one component of
    /// kind processor or exporter that supports `OtlpProtoBytes`.
    #[must_use]
    pub fn has_supported_phase1_component(&self) -> bool {
        self.components.iter().any(|c| {
            matches!(c.kind, ComponentKind::Processor | ComponentKind::Exporter)
                && c.supported_payloads
                    .iter()
                    .any(|f| matches!(f, PayloadFormat::OtlpProtoBytes))
        })
    }
}
