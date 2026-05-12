// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Native plugin processor adapter.
//!
//! Implements the engine's `local::processor::Processor<OtapPdata>` and
//! routes each `Message::PData` through the plugin's `process` export.
//!
//! The adapter does **not** copy or serialize the payload. The plugin
//! receives only an opaque [`OtapPdataHandle`](otap_df_plugin_abi::OtapPdataHandle)
//! borrowed for the duration of the `process` call.
//!
//! Verb mapping:
//! * `ForwardSame` ⇒ emit the original `OtapPdata` to the default output.
//! * `Drop`        ⇒ filter (no emit).
//! * `Error`       ⇒ raise `Error::ProcessorError` with the plugin's
//!   message.

use std::sync::Arc;

use async_trait::async_trait;

use otap_df_engine::error::{Error, ProcessorErrorKind};
use otap_df_engine::local::processor::{EffectHandler, Processor};
use otap_df_engine::message::Message;
use otap_df_engine::node::NodeId;
use otap_df_otap::pdata::OtapPdata;
use otap_df_plugin_api::PluginFingerprint;
use otap_df_plugin_native_host::{
    NativeCacheKey, NativeProcessorRunner, NativeVerb, runner::NativeProcessorRunnerImpl,
};

/// Plugin-backed processor adapter.
///
/// One adapter instance per (node, core). Holds an
/// [`Arc<NativeProcessorRunnerImpl>`] (the per-node plugin instance).
pub struct NativeProcessorAdapter {
    /// Component URN this adapter is bound to.
    pub component_urn: Arc<str>,
    /// Plugin artifact identity (rollout fingerprint).
    pub fingerprint: PluginFingerprint,
    /// Cache key for the loaded artifact.
    pub cache_key: NativeCacheKey,
    /// Pre-serialized JSON config (already accepted by `validate-config`).
    pub config_json: String,
    /// Backing per-node runner.
    pub runner: Arc<NativeProcessorRunnerImpl>,
    /// Node identity for error reporting.
    pub node_id: NodeId,
}

impl std::fmt::Debug for NativeProcessorAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeProcessorAdapter")
            .field("component_urn", &self.component_urn)
            .field("fingerprint", &self.fingerprint)
            .field("cache_key", &self.cache_key)
            .finish_non_exhaustive()
    }
}

impl NativeProcessorAdapter {
    fn proc_err(&self, kind: ProcessorErrorKind, msg: impl Into<String>) -> Error {
        Error::ProcessorError {
            processor: self.node_id.clone(),
            kind,
            error: msg.into(),
            source_detail: String::new(),
        }
    }

    /// Apply the plugin to a single pdata message and decide what to do
    /// with it. Returns `Ok(Some(pdata))` to emit the original pdata
    /// downstream, `Ok(None)` to drop, `Err(_)` on plugin Error verb.
    ///
    /// Unit-test entry point — the engine path goes through
    /// [`Processor::process`] below, which calls this and forwards the
    /// result through the effect handler.
    pub fn dispatch(&self, pdata: OtapPdata) -> Result<Option<OtapPdata>, Error> {
        let verb = self.runner.process(&pdata);
        match verb {
            NativeVerb::ForwardSame => Ok(Some(pdata)),
            NativeVerb::Drop => Ok(None),
            NativeVerb::Error(msg) => Err(self.proc_err(ProcessorErrorKind::Other, msg)),
        }
    }
}

#[async_trait(?Send)]
impl Processor<OtapPdata> for NativeProcessorAdapter {
    async fn process(
        &mut self,
        msg: Message<OtapPdata>,
        effect_handler: &mut EffectHandler<OtapPdata>,
    ) -> Result<(), Error> {
        let pdata = match msg {
            Message::PData(p) => p,
            // Plugin contract is data-only; control messages are
            // engine-managed (shutdown / timer / wakeup / etc.).
            Message::Control(_) => return Ok(()),
        };

        match self.dispatch(pdata)? {
            Some(out) => effect_handler.send_message(out).await.map_err(|e| {
                self.proc_err(
                    ProcessorErrorKind::Transport,
                    format!("default-port send failed: {e}"),
                )
            }),
            None => Ok(()),
        }
    }
}
