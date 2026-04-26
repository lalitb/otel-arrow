// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Runtime component registry overlay.
//!
//! Phase-1 scaffolding for the dynamic plugin RFC
//! (`docs/dynamic-processor-exporter-plugins-phase1.md`).
//!
//! The static `PipelineFactory` keeps its `linkme`-based registration as the
//! first-class path. This module adds an additive overlay that:
//!   * exposes the static factory through the same lookup surface, and
//!   * carries an owned, runtime-loaded set of processor/exporter entries
//!     contributed by dynamic plugins.
//!
//! Lookup precedence is **static-first, dynamic-second** so a plugin URN can
//! never shadow a built-in.
//!
//! Phase 1 explicitly scopes dynamic registration to *processors* and
//! *exporters*. Receivers and extensions are not supported via plugins yet
//! and are intentionally absent from the dynamic side of this overlay.

use std::collections::HashMap;
use std::fmt::{self, Debug};
use std::sync::Arc;

use serde_json::Value;

use crate::PipelineFactory;
use crate::config::{ExporterConfig, ProcessorConfig};
use crate::context::PipelineContext;
use crate::exporter::ExporterWrapper;
use crate::node::NodeId;
use crate::processor::ProcessorWrapper;
use crate::wiring_contract::WiringContract;
use otap_df_config::node::NodeUserConfig;

/// Identifier the host expects on plugin-backed components.
///
/// Borrows the same URN string identity used by static built-ins
/// (`urn:<namespace>:<kind>:<id>`). Owned at runtime because dynamic entries
/// are loaded after binary start.
pub type ComponentUrn = Arc<str>;

/// Validator for a node's user-supplied JSON config.
///
/// Static built-ins use plain `fn` pointers (current behavior). Dynamic
/// components need to keep the host alive across the call (e.g. a Wasmtime
/// instance), so the dynamic variant boxes a `Send + Sync` callable.
#[derive(Clone)]
pub enum ConfigValidator {
    /// A static-lifetime function pointer, identical to today's built-in
    /// `validate_config` field on factory structs.
    Static(fn(&Value) -> Result<(), otap_df_config::error::Error>),
    /// A dynamic, owned validator. Plugin-backed entries put a closure here
    /// that calls into the plugin's `validate-config` export.
    Dynamic(Arc<dyn Fn(&Value) -> Result<(), otap_df_config::error::Error> + Send + Sync>),
}

impl ConfigValidator {
    /// Run the validator against a JSON config value.
    pub fn validate(&self, config: &Value) -> Result<(), otap_df_config::error::Error> {
        match self {
            ConfigValidator::Static(f) => f(config),
            ConfigValidator::Dynamic(f) => f(config),
        }
    }
}

impl Debug for ConfigValidator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigValidator::Static(_) => f.write_str("ConfigValidator::Static(<fn>)"),
            ConfigValidator::Dynamic(_) => f.write_str("ConfigValidator::Dynamic(<arc>)"),
        }
    }
}

/// Opaque identity for a plugin artifact, carried alongside dynamic entries.
///
/// Used by live reconfiguration to detect plugin upgrades: a change to any
/// field counts as a runtime-shape change for the affected pipeline.
///
/// The exact shape is mirrored in `otap-df-plugin-api`; we re-declare a
/// minimal version here so `otap-df-engine` does not depend on the plugin
/// crates.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DynamicNodeFingerprint {
    /// Component URN, e.g. `urn:acme:processor:redact`.
    pub component_urn: String,
    /// Plugin manifest version (e.g. `0.1.0`).
    pub plugin_version: String,
    /// Hex-encoded SHA-256 of the plugin artifact.
    pub artifact_sha256: String,
    /// Plugin API version the plugin was built against.
    pub plugin_api_version: String,
}

/// Boxed dynamic-processor factory closure.
///
/// Mirrors the inputs of `ProcessorFactory::create` (a fn-pointer for static
/// built-ins) but uses a `dyn Fn` so plugin-backed factories can capture
/// per-component state — most notably a Wasmtime `LoadedComponent` handle
/// owned by the plugin host.
pub type DynamicProcessorFactory<PData> = Arc<
    dyn Fn(
            PipelineContext,
            NodeId,
            Arc<NodeUserConfig>,
            &ProcessorConfig,
        ) -> Result<ProcessorWrapper<PData>, otap_df_config::error::Error>
        + Send
        + Sync,
>;

/// Boxed dynamic-exporter factory closure (same rationale as
/// [`DynamicProcessorFactory`]).
pub type DynamicExporterFactory<PData> = Arc<
    dyn Fn(
            PipelineContext,
            NodeId,
            Arc<NodeUserConfig>,
            &ExporterConfig,
        ) -> Result<ExporterWrapper<PData>, otap_df_config::error::Error>
        + Send
        + Sync,
>;

/// A dynamically registered processor entry (phase 1 scaffolding).
///
/// The `factory` field is intentionally `Option`. When present, pipeline
/// build invokes it instead of the static factory map. When `None`, any
/// pipeline referencing this URN is rejected at build time — this is the
/// honest failure model for the case where a plugin loaded its descriptor
/// and validator successfully but the runtime backend (Wasmtime) is not
/// compiled in.
pub struct DynamicProcessorEntry<PData: 'static + Clone> {
    /// Owned URN string — dynamic entries cannot use `&'static str`.
    pub urn: ComponentUrn,
    /// Validator for the node's user config.
    pub validator: ConfigValidator,
    /// Plugin artifact identity (rollout fingerprint).
    pub fingerprint: DynamicNodeFingerprint,
    /// Wiring constraints. Default is unrestricted; phase-1 plugin
    /// processors are also enforced to be single/default-output by the
    /// descriptor verifier in `plugin-host`, so this field is a hook for
    /// future tightening rather than a hard constraint today.
    pub wiring_contract: WiringContract,
    /// Optional concrete factory. `Some(_)` once the plugin host has
    /// wired up a Wasmtime-backed adapter; `None` if the descriptor was
    /// loaded but no runtime backend is available (e.g. the
    /// `wasmtime-backend` feature is not compiled in). Pipelines
    /// referencing a `None`-factory URN are rejected at pre-flight
    /// validation.
    pub factory: Option<DynamicProcessorFactory<PData>>,
}

impl<PData: 'static + Clone> Clone for DynamicProcessorEntry<PData> {
    fn clone(&self) -> Self {
        Self {
            urn: self.urn.clone(),
            validator: self.validator.clone(),
            fingerprint: self.fingerprint.clone(),
            wiring_contract: self.wiring_contract,
            factory: self.factory.clone(),
        }
    }
}

impl<PData: 'static + Clone> Debug for DynamicProcessorEntry<PData> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DynamicProcessorEntry")
            .field("urn", &self.urn)
            .field("validator", &self.validator)
            .field("fingerprint", &self.fingerprint)
            .field("wiring_contract", &self.wiring_contract)
            .field("factory", &self.factory.is_some())
            .finish()
    }
}

/// A dynamically registered exporter entry (phase 1 scaffolding).
pub struct DynamicExporterEntry<PData: 'static + Clone> {
    /// Owned URN string.
    pub urn: ComponentUrn,
    /// Validator for the node's user config.
    pub validator: ConfigValidator,
    /// Plugin artifact identity (rollout fingerprint).
    pub fingerprint: DynamicNodeFingerprint,
    /// Wiring constraints (see [`DynamicProcessorEntry::wiring_contract`]).
    pub wiring_contract: WiringContract,
    /// Optional concrete factory. See [`DynamicProcessorEntry::factory`].
    pub factory: Option<DynamicExporterFactory<PData>>,
}

impl<PData: 'static + Clone> Clone for DynamicExporterEntry<PData> {
    fn clone(&self) -> Self {
        Self {
            urn: self.urn.clone(),
            validator: self.validator.clone(),
            fingerprint: self.fingerprint.clone(),
            wiring_contract: self.wiring_contract,
            factory: self.factory.clone(),
        }
    }
}

impl<PData: 'static + Clone> Debug for DynamicExporterEntry<PData> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DynamicExporterEntry")
            .field("urn", &self.urn)
            .field("validator", &self.validator)
            .field("fingerprint", &self.fingerprint)
            .field("wiring_contract", &self.wiring_contract)
            .field("factory", &self.factory.is_some())
            .finish()
    }
}

/// Owned, mutable container for the dynamic side of the overlay.
///
/// Built once at startup (or rebuilt at rollout time) by the plugin host.
/// Engine code never mutates this — it only reads via
/// [`RuntimeComponentRegistry`].
#[derive(Default)]
pub struct DynamicComponentRegistry<PData: 'static + Clone> {
    processors: HashMap<ComponentUrn, DynamicProcessorEntry<PData>>,
    exporters: HashMap<ComponentUrn, DynamicExporterEntry<PData>>,
}

impl<PData: 'static + Clone> DynamicComponentRegistry<PData> {
    /// Construct an empty dynamic registry (the default for distributions
    /// that do not load any plugins).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            processors: HashMap::new(),
            exporters: HashMap::new(),
        }
    }

    /// Register a dynamic processor entry.
    ///
    /// Returns `Err` with the offending URN if it is already registered.
    /// Static built-ins are *not* checked here — the
    /// [`RuntimeComponentRegistry`] enforces static-first precedence at
    /// lookup time.
    pub fn register_processor(
        &mut self,
        entry: DynamicProcessorEntry<PData>,
    ) -> Result<(), DuplicateUrn> {
        let key = entry.urn.clone();
        if self.processors.contains_key(&key) {
            return Err(DuplicateUrn(key.to_string()));
        }
        let _ = self.processors.insert(key, entry);
        Ok(())
    }

    /// Register a dynamic exporter entry.
    pub fn register_exporter(
        &mut self,
        entry: DynamicExporterEntry<PData>,
    ) -> Result<(), DuplicateUrn> {
        let key = entry.urn.clone();
        if self.exporters.contains_key(&key) {
            return Err(DuplicateUrn(key.to_string()));
        }
        let _ = self.exporters.insert(key, entry);
        Ok(())
    }

    /// Iterate over registered processor entries.
    pub fn processors(&self) -> impl Iterator<Item = &DynamicProcessorEntry<PData>> {
        self.processors.values()
    }

    /// Iterate over registered exporter entries.
    pub fn exporters(&self) -> impl Iterator<Item = &DynamicExporterEntry<PData>> {
        self.exporters.values()
    }

    /// Look up a dynamic processor by URN.
    #[must_use]
    pub fn processor(&self, urn: &str) -> Option<&DynamicProcessorEntry<PData>> {
        self.processors.get(urn)
    }

    /// Look up a dynamic exporter by URN.
    #[must_use]
    pub fn exporter(&self, urn: &str) -> Option<&DynamicExporterEntry<PData>> {
        self.exporters.get(urn)
    }

    /// Total registered dynamic component count, used in startup banner.
    #[must_use]
    pub fn len(&self) -> usize {
        self.processors.len() + self.exporters.len()
    }

    /// Convenience: is the dynamic side empty?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.processors.is_empty() && self.exporters.is_empty()
    }
}

impl<PData: 'static + Clone> Debug for DynamicComponentRegistry<PData> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DynamicComponentRegistry")
            .field("processors", &self.processors.keys().collect::<Vec<_>>())
            .field("exporters", &self.exporters.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Combined view over the static [`PipelineFactory`] and a dynamic registry.
///
/// All validation/lookup paths added in phase 1 should accept this type,
/// not raw `&PipelineFactory`. Static-only call sites can keep using
/// `&PipelineFactory` directly; this type is purely additive.
pub struct RuntimeComponentRegistry<PData: 'static + Clone> {
    static_factory: &'static PipelineFactory<PData>,
    dynamic: Arc<DynamicComponentRegistry<PData>>,
}

impl<PData: 'static + Clone> Clone for RuntimeComponentRegistry<PData> {
    fn clone(&self) -> Self {
        Self {
            static_factory: self.static_factory,
            dynamic: Arc::clone(&self.dynamic),
        }
    }
}

impl<PData: 'static + Clone> RuntimeComponentRegistry<PData> {
    /// Build an overlay with a static factory and an owned dynamic registry.
    #[must_use]
    pub fn new(
        static_factory: &'static PipelineFactory<PData>,
        dynamic: DynamicComponentRegistry<PData>,
    ) -> Self {
        Self {
            static_factory,
            dynamic: Arc::new(dynamic),
        }
    }

    /// Build an overlay with no dynamic entries — equivalent to today's
    /// static-only behavior.
    #[must_use]
    pub fn static_only(static_factory: &'static PipelineFactory<PData>) -> Self {
        Self::new(static_factory, DynamicComponentRegistry::empty())
    }

    /// The wrapped static factory (for callers that still need direct
    /// access during pipeline build).
    #[must_use]
    pub fn static_factory(&self) -> &'static PipelineFactory<PData> {
        self.static_factory
    }

    /// The owned dynamic registry.
    #[must_use]
    pub fn dynamic(&self) -> &DynamicComponentRegistry<PData> {
        &self.dynamic
    }

    /// The owned dynamic registry behind the internal `Arc`. Useful for
    /// long-lived consumers (e.g. controller subsystems) that need their own
    /// Arc clone without going through the overlay.
    #[must_use]
    pub fn dynamic_arc(&self) -> &Arc<DynamicComponentRegistry<PData>> {
        &self.dynamic
    }
}

impl<PData: 'static + Clone + Debug> RuntimeComponentRegistry<PData> {
    /// Look up the validator for a receiver. Receivers are static-only in
    /// phase 1 — the dynamic side never carries them.
    #[must_use]
    pub fn receiver_validator(&self, urn: &str) -> Option<ConfigValidator> {
        self.static_factory
            .get_receiver_factory_map()
            .get(urn)
            .map(|f| ConfigValidator::Static(f.validate_config))
    }

    /// Look up the validator for a processor URN, checking static built-ins
    /// first and then the dynamic overlay.
    #[must_use]
    pub fn processor_validator(&self, urn: &str) -> Option<ConfigValidator> {
        if let Some(f) = self.static_factory.get_processor_factory_map().get(urn) {
            return Some(ConfigValidator::Static(f.validate_config));
        }
        self.dynamic.processor(urn).map(|e| e.validator.clone())
    }

    /// Look up the validator for an exporter URN, static-first then dynamic.
    #[must_use]
    pub fn exporter_validator(&self, urn: &str) -> Option<ConfigValidator> {
        if let Some(f) = self.static_factory.get_exporter_factory_map().get(urn) {
            return Some(ConfigValidator::Static(f.validate_config));
        }
        self.dynamic.exporter(urn).map(|e| e.validator.clone())
    }

    /// Whether a processor URN is known (in either static or dynamic side).
    #[must_use]
    pub fn knows_processor(&self, urn: &str) -> bool {
        self.static_factory
            .get_processor_factory_map()
            .contains_key(urn)
            || self.dynamic.processor(urn).is_some()
    }

    /// Whether an exporter URN is known.
    #[must_use]
    pub fn knows_exporter(&self, urn: &str) -> bool {
        self.static_factory
            .get_exporter_factory_map()
            .contains_key(urn)
            || self.dynamic.exporter(urn).is_some()
    }

    /// Whether a processor URN is plugin-backed (dynamic). False for static.
    #[must_use]
    pub fn is_processor_dynamic(&self, urn: &str) -> bool {
        // Static-first precedence: if it's also static, treat as static.
        if self
            .static_factory
            .get_processor_factory_map()
            .contains_key(urn)
        {
            return false;
        }
        self.dynamic.processor(urn).is_some()
    }

    /// Whether an exporter URN is plugin-backed (dynamic).
    #[must_use]
    pub fn is_exporter_dynamic(&self, urn: &str) -> bool {
        if self
            .static_factory
            .get_exporter_factory_map()
            .contains_key(urn)
        {
            return false;
        }
        self.dynamic.exporter(urn).is_some()
    }

    /// Fingerprint for a dynamic processor URN, if any. Used by live
    /// reconfiguration when computing pipeline runtime shape.
    #[must_use]
    pub fn processor_fingerprint(&self, urn: &str) -> Option<&DynamicNodeFingerprint> {
        self.dynamic.processor(urn).map(|e| &e.fingerprint)
    }

    /// Fingerprint for a dynamic exporter URN, if any.
    #[must_use]
    pub fn exporter_fingerprint(&self, urn: &str) -> Option<&DynamicNodeFingerprint> {
        self.dynamic.exporter(urn).map(|e| &e.fingerprint)
    }

    /// Wiring contract for a processor URN, static-first then dynamic.
    /// Returns `None` if neither side knows the URN.
    #[must_use]
    pub fn processor_wiring_contract(&self, urn: &str) -> Option<WiringContract> {
        if let Some(f) = self.static_factory.get_processor_factory_map().get(urn) {
            return Some(f.wiring_contract);
        }
        self.dynamic.processor(urn).map(|e| e.wiring_contract)
    }

    /// Wiring contract for an exporter URN, static-first then dynamic.
    #[must_use]
    pub fn exporter_wiring_contract(&self, urn: &str) -> Option<WiringContract> {
        if let Some(f) = self.static_factory.get_exporter_factory_map().get(urn) {
            return Some(f.wiring_contract);
        }
        self.dynamic.exporter(urn).map(|e| e.wiring_contract)
    }

    /// Dynamic processor factory for a URN, if (a) the URN is plugin-backed
    /// and (b) the factory is wired up. The pipeline build path consults
    /// this only when the static map misses, preserving static-first
    /// precedence.
    #[must_use]
    pub fn dynamic_processor_factory(&self, urn: &str) -> Option<DynamicProcessorFactory<PData>> {
        self.dynamic.processor(urn).and_then(|e| e.factory.clone())
    }

    /// Dynamic exporter factory for a URN.
    #[must_use]
    pub fn dynamic_exporter_factory(&self, urn: &str) -> Option<DynamicExporterFactory<PData>> {
        self.dynamic.exporter(urn).and_then(|e| e.factory.clone())
    }
}

/// Returned when a duplicate URN is registered into the dynamic side.
#[derive(Debug, thiserror::Error)]
#[error("duplicate plugin component URN: {0}")]
pub struct DuplicateUrn(pub String);

#[cfg(test)]
mod tests {
    use super::*;
    use otap_df_config::error::Error as ConfigError;

    fn ok_validator(_: &Value) -> Result<(), ConfigError> {
        Ok(())
    }

    fn fp(urn: &str) -> DynamicNodeFingerprint {
        DynamicNodeFingerprint {
            component_urn: urn.to_string(),
            plugin_version: "0.1.0".into(),
            artifact_sha256: "0".repeat(64),
            plugin_api_version: "0.1".into(),
        }
    }

    #[test]
    fn duplicate_dynamic_processor_rejected() {
        let mut dyn_reg: DynamicComponentRegistry<()> = DynamicComponentRegistry::empty();
        let entry = DynamicProcessorEntry::<()> {
            urn: Arc::from("urn:test:processor:foo"),
            validator: ConfigValidator::Static(ok_validator),
            fingerprint: fp("urn:test:processor:foo"),
            wiring_contract: WiringContract::UNRESTRICTED,
            factory: None,
        };
        dyn_reg.register_processor(entry.clone()).unwrap();
        let err = dyn_reg.register_processor(entry).unwrap_err();
        assert!(err.0.contains("urn:test:processor:foo"));
    }

    #[test]
    fn dynamic_validator_runs() {
        fn fail(_: &Value) -> Result<(), ConfigError> {
            Err(ConfigError::InvalidUserConfig {
                error: "nope".into(),
            })
        }
        let v = ConfigValidator::Static(fail);
        assert!(v.validate(&Value::Null).is_err());

        let v2 = ConfigValidator::Dynamic(Arc::new(|_| Ok(())));
        assert!(v2.validate(&Value::Null).is_ok());
    }
}
