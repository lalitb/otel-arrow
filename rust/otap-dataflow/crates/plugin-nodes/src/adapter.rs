// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Plugin-backed adapter nodes implementing the engine's local
//! `Processor` and `Exporter` traits.
//!
//! Phase-1 execution model:
//!   * processor: synchronous Wasmtime call inline on the per-core runtime
//!     (host-enforced deadline is the safety net — see RFC §6.2).
//!   * exporter: synchronous Wasmtime call inside the exporter `start`
//!     loop. Per-instance serialization is automatic because the exporter
//!     owns its store via `Box<Self>` in `start`.
//!   * payload: only [`PayloadKind::OtlpProtoBytes`]. Arrow IPC is
//!     reserved in the ABI but rejected at conversion time today.
//!   * routing: emits to the **default** output port only (RFC §3.4).
//!
//! Plugin result classes (see [`otap_df_plugin_host::result_class`]) are
//! mapped into engine semantics:
//!   * `OK + non-empty` → emit; `OK + empty` → no-op
//!   * `DROP`           → filter (no emit)
//!   * `RETRYABLE` /    → return [`Error::ProcessorError`] /
//!     `PERMANENT`        [`Error::ExporterError`]
//!   * any other class  → treated as `PERMANENT`
//!
//! Phase 1 does not differentiate retryable from permanent at the engine
//! layer; both surface as node errors. Future patches can wire retryable
//! into NACK semantics once the exporter retry path supports it.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use otap_df_engine::error::{Error, ExporterErrorKind, ProcessorErrorKind};
use otap_df_engine::local::exporter::{EffectHandler as ExporterEffectHandler, Exporter};
use otap_df_engine::local::processor::{EffectHandler as ProcessorEffectHandler, Processor};
use otap_df_engine::message::{ExporterInbox, Message};
use otap_df_engine::node::NodeId;
use otap_df_engine::terminal_state::TerminalState;
use otap_df_otap::pdata::Context;
use otap_df_otap::pdata::OtapPdata;
use otap_df_pdata::OtapPayload;
use otap_df_pdata::OtlpProtoBytes;
use otap_df_plugin_api::PluginFingerprint;
use otap_df_plugin_host::{
    ComponentCacheKey, PayloadKind, PluginRunner,
    result_class::{DROP, OK, PERMANENT, RETRYABLE},
};
use otap_df_plugin_manifest::Limits;

use otap_df_config::SignalType;

/// Numeric signal tag the plugin contract expects.
fn signal_tag(s: SignalType) -> u32 {
    match s {
        SignalType::Logs => 0,
        SignalType::Metrics => 1,
        SignalType::Traces => 2,
    }
}

/// Decode the plugin's emitted bytes back into an [`OtapPdata`] using the
/// signal type of the original message. The original `Context` is moved
/// in so transport headers, lineage, and other host-managed envelope
/// state survive the plugin hop (the plugin transforms payload bytes,
/// not host envelope state).
fn rebuild_pdata(signal: SignalType, bytes: Vec<u8>, ctx: Context) -> OtapPdata {
    let proto = match signal {
        SignalType::Logs => OtlpProtoBytes::ExportLogsRequest(Bytes::from(bytes)),
        SignalType::Metrics => OtlpProtoBytes::ExportMetricsRequest(Bytes::from(bytes)),
        SignalType::Traces => OtlpProtoBytes::ExportTracesRequest(Bytes::from(bytes)),
    };
    OtapPdata::new(ctx, proto.into())
}

/// Convert an [`OtapPayload`] into bytes the plugin understands.
///
/// Phase 1 only supports OTLP-proto-bytes. Arrow records are re-encoded
/// into OTLP bytes (this is what `TryFrom<OtapPayload> for OtlpProtoBytes`
/// already does in `pdata`). If the conversion fails (e.g. bad Arrow
/// schema), the caller propagates a clear error.
fn payload_to_otlp_bytes(payload: OtapPayload) -> Result<(SignalType, Vec<u8>), String> {
    let signal = match &payload {
        OtapPayload::OtlpBytes(b) => match b {
            OtlpProtoBytes::ExportLogsRequest(_) => SignalType::Logs,
            OtlpProtoBytes::ExportMetricsRequest(_) => SignalType::Metrics,
            OtlpProtoBytes::ExportTracesRequest(_) => SignalType::Traces,
        },
        OtapPayload::OtapArrowRecords(r) => match r {
            otap_df_pdata::OtapArrowRecords::Logs(_) => SignalType::Logs,
            otap_df_pdata::OtapArrowRecords::Metrics(_) => SignalType::Metrics,
            otap_df_pdata::OtapArrowRecords::Traces(_) => SignalType::Traces,
        },
    };
    let proto: OtlpProtoBytes = payload
        .try_into()
        .map_err(|e| format!("payload→OTLP bytes conversion failed: {e}"))?;
    let bytes: Vec<u8> = match proto {
        OtlpProtoBytes::ExportLogsRequest(b)
        | OtlpProtoBytes::ExportMetricsRequest(b)
        | OtlpProtoBytes::ExportTracesRequest(b) => b.to_vec(),
    };
    Ok((signal, bytes))
}

/// Plugin-backed processor.
///
/// One adapter instance per (node, core). Holds a shared [`PluginRunner`]
/// (cheap clone — internally Arc-backed) and the descriptor metadata used
/// for diagnostics.
pub struct WasmProcessorAdapter {
    /// Component URN this adapter is bound to.
    pub component_urn: Arc<str>,
    /// Plugin artifact identity (rollout fingerprint).
    pub fingerprint: PluginFingerprint,
    /// Cache key for the precompiled component.
    pub cache_key: ComponentCacheKey,
    /// Execution limits (see [`Limits`]).
    pub limits: Limits,
    /// Pre-serialized JSON config the plugin already accepted via
    /// `validate-config`.
    pub config_json: String,
    /// Backing runner.
    pub runner: Arc<dyn PluginRunner>,
    /// Node identity for error reporting.
    pub node_id: NodeId,
}

impl std::fmt::Debug for WasmProcessorAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmProcessorAdapter")
            .field("component_urn", &self.component_urn)
            .field("fingerprint", &self.fingerprint)
            .field("cache_key", &self.cache_key)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl WasmProcessorAdapter {
    fn proc_err(&self, kind: ProcessorErrorKind, msg: impl Into<String>) -> Error {
        Error::ProcessorError {
            processor: self.node_id.clone(),
            kind,
            error: msg.into(),
            source_detail: String::new(),
        }
    }

    /// Apply the plugin to a single pdata message, returning what (if
    /// anything) the engine should emit to the default output.
    ///
    /// `Ok(Some(pdata))` — emit the rebuilt pdata (carries the original
    /// `Context` to preserve transport headers and routing lineage).
    /// `Ok(None)`        — drop / no-op (plugin returned `Drop` or
    /// `Ok` with empty bytes).
    /// `Err(_)`          — plugin returned a retryable/permanent error
    /// or an unexpected class.
    pub(crate) fn transform(&self, pdata: OtapPdata) -> Result<Option<OtapPdata>, Error> {
        let (ctx, payload) = pdata.into_parts();
        let (signal, bytes) = payload_to_otlp_bytes(payload)
            .map_err(|e| self.proc_err(ProcessorErrorKind::Other, e))?;

        // Phase-1 inline call. Bounded by the plugin's host-enforced
        // deadline (RFC §6.2): Wasmtime epoch interruption traps the
        // call after `self.limits.timeout_ms` milliseconds.
        let (class, emitted) = self
            .runner
            .process(
                signal_tag(signal),
                PayloadKind::OtlpProtoBytes,
                &bytes,
                &self.config_json,
                self.limits.timeout_ms,
            )
            .map_err(|e| self.proc_err(ProcessorErrorKind::Other, e))?;

        match class {
            OK => {
                if emitted.is_empty() {
                    // Treat Ok+empty as no-op (semantically identical
                    // to Drop). Avoids constructing a zero-length OTLP
                    // request that downstream nodes would have to
                    // ignore.
                    Ok(None)
                } else {
                    Ok(Some(rebuild_pdata(signal, emitted, ctx)))
                }
            }
            DROP => Ok(None),
            RETRYABLE => Err(self.proc_err(
                ProcessorErrorKind::Other,
                "plugin returned retryable error class",
            )),
            PERMANENT => Err(self.proc_err(
                ProcessorErrorKind::Other,
                "plugin returned permanent error class",
            )),
            other => Err(self.proc_err(
                ProcessorErrorKind::Other,
                format!("plugin returned unknown result class: {other}"),
            )),
        }
    }
}

#[async_trait(?Send)]
impl Processor<OtapPdata> for WasmProcessorAdapter {
    async fn process(
        &mut self,
        msg: Message<OtapPdata>,
        effect_handler: &mut ProcessorEffectHandler<OtapPdata>,
    ) -> Result<(), Error> {
        let pdata = match msg {
            Message::PData(p) => p,
            // Plugin contract is data-only; control messages are no-ops at
            // the adapter layer (the engine handles shutdown/timer/etc.
            // around us).
            Message::Control(_) => return Ok(()),
        };

        match self.transform(pdata)? {
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

/// Plugin-backed exporter.
///
/// The engine takes ownership of the adapter via `Box<Self>` in `start`,
/// so per-instance serialization is automatic — there is no way for the
/// engine to call this adapter concurrently with itself.
///
/// Cross-instance concurrency is bounded by `blocking_permits` (a shared
/// [`tokio::sync::Semaphore`] sized by
/// [`otap_df_plugin_host::PluginHostConfig::exporter_blocking_concurrency`]).
/// `start` acquires one permit per `process` dispatch and releases it
/// only after the `spawn_blocking` join completes, which provides
/// host-wide back-pressure under saturation.
pub struct WasmExporterAdapter {
    /// Component URN this adapter is bound to.
    pub component_urn: Arc<str>,
    /// Plugin artifact identity.
    pub fingerprint: PluginFingerprint,
    /// Cache key for the precompiled component.
    pub cache_key: ComponentCacheKey,
    /// Execution limits.
    pub limits: Limits,
    /// Pre-serialized JSON config.
    pub config_json: String,
    /// Backing runner.
    pub runner: Arc<dyn PluginRunner>,
    /// Node identity for error reporting.
    pub node_id: NodeId,
    /// Shared concurrency cap for plugin exporter blocking-pool dispatch.
    /// Cloned once from [`PluginHost::exporter_blocking_permits`] when the
    /// dynamic factory is built.
    pub blocking_permits: Arc<tokio::sync::Semaphore>,
}

impl std::fmt::Debug for WasmExporterAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmExporterAdapter")
            .field("component_urn", &self.component_urn)
            .field("fingerprint", &self.fingerprint)
            .field("cache_key", &self.cache_key)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl WasmExporterAdapter {
    fn exp_err(&self, kind: ExporterErrorKind, msg: impl Into<String>) -> Error {
        Error::ExporterError {
            exporter: self.node_id.clone(),
            kind,
            error: msg.into(),
            source_detail: String::new(),
        }
    }

    /// Dispatch one plugin `process` call through the bounded blocking
    /// pool, acquiring (and dropping) one permit from
    /// `blocking_permits` around the `spawn_blocking` join.
    ///
    /// Extracted from [`Exporter::start`] so cap-enforcement and
    /// closed-semaphore behavior are testable without standing up a full
    /// engine pipeline. The `start` loop calls this for each inbound
    /// `PData` message.
    pub(crate) async fn dispatch_one(
        runner: Arc<dyn PluginRunner>,
        permits: Arc<tokio::sync::Semaphore>,
        signal_u32: u32,
        bytes: Vec<u8>,
        config_json: String,
        timeout_ms: u64,
    ) -> Result<(u32, Vec<u8>), DispatchError> {
        let permit = permits
            .acquire_owned()
            .await
            .map_err(|e| DispatchError::PermitAcquire(format!("{e}")))?;
        let join = tokio::task::spawn_blocking(move || {
            runner.process(
                signal_u32,
                PayloadKind::OtlpProtoBytes,
                &bytes,
                &config_json,
                timeout_ms,
            )
        });
        let join_result = join.await;
        drop(permit);
        let plugin_result = join_result
            .map_err(|e| DispatchError::JoinFailed(format!("{e}")))?
            .map_err(DispatchError::PluginRuntime)?;
        Ok(plugin_result)
    }
}

/// Outcome categories from [`WasmExporterAdapter::dispatch_one`]. The
/// caller maps these into engine `ExporterError` variants — keeping that
/// mapping out of the dispatch helper lets tests assert on the dispatch
/// layer without running through engine error types.
#[derive(Debug)]
pub(crate) enum DispatchError {
    /// The shared semaphore was closed while we were waiting for a permit.
    PermitAcquire(String),
    /// `tokio::task::spawn_blocking` failed to join (panic, runtime gone).
    JoinFailed(String),
    /// The plugin runner returned `Err` (host-side trap, deadline exceeded, etc.).
    PluginRuntime(String),
}

#[async_trait(?Send)]
impl Exporter<OtapPdata> for WasmExporterAdapter {
    async fn start(
        self: Box<Self>,
        mut inbox: ExporterInbox<OtapPdata>,
        _effect_handler: ExporterEffectHandler<OtapPdata>,
    ) -> Result<TerminalState, Error> {
        loop {
            match inbox.recv().await {
                Ok(Message::PData(pdata)) => {
                    let (_ctx, payload) = pdata.into_parts();
                    let (signal, bytes) = payload_to_otlp_bytes(payload)
                        .map_err(|e| self.exp_err(ExporterErrorKind::Other, e))?;

                    // RFC §6.2: exporter execution dispatches the plugin
                    // call through a bounded blocking worker so a slow
                    // plugin call cannot stall the per-core runtime.
                    // Concurrency is capped explicitly via the shared
                    // `blocking_permits` semaphore (see
                    // `PluginHostConfig::exporter_blocking_concurrency`).
                    // Saturation behavior is back-pressure: the exporter
                    // loop suspends in `acquire_owned()`, slowing inbox
                    // drain without dropping data.
                    //
                    // Per-instance serialization is automatic — the
                    // engine takes ownership of the adapter via
                    // `Box<Self>`, and we await the dispatch before
                    // pulling the next message.
                    let (class, _emitted) = WasmExporterAdapter::dispatch_one(
                        Arc::clone(&self.runner),
                        Arc::clone(&self.blocking_permits),
                        signal_tag(signal),
                        bytes,
                        self.config_json.clone(),
                        self.limits.timeout_ms,
                    )
                    .await
                    .map_err(|e| match e {
                        DispatchError::PermitAcquire(msg) => self.exp_err(
                            ExporterErrorKind::Other,
                            format!("plugin blocking-permit acquisition failed: {msg}"),
                        ),
                        DispatchError::JoinFailed(msg) => self.exp_err(
                            ExporterErrorKind::Other,
                            format!("blocking worker join failed: {msg}"),
                        ),
                        DispatchError::PluginRuntime(msg) => {
                            self.exp_err(ExporterErrorKind::Transport, msg)
                        }
                    })?;

                    match class {
                        OK | DROP => {
                            // Exporters do not re-emit; plugin-emitted
                            // bytes (if any) are ignored by phase-1
                            // contract.
                        }
                        RETRYABLE => {
                            return Err(self.exp_err(
                                ExporterErrorKind::Transport,
                                "plugin returned retryable error class",
                            ));
                        }
                        PERMANENT => {
                            return Err(self.exp_err(
                                ExporterErrorKind::Transport,
                                "plugin returned permanent error class",
                            ));
                        }
                        other => {
                            return Err(self.exp_err(
                                ExporterErrorKind::Other,
                                format!("plugin returned unknown result class: {other}"),
                            ));
                        }
                    }
                }
                Ok(Message::Control(c)) if c.is_shutdown() => {
                    return Ok(TerminalState::default());
                }
                Ok(Message::Control(_)) => {}
                Err(_) => return Ok(TerminalState::default()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otap_df_pdata::OtlpProtoBytes;

    #[test]
    fn signal_tags_match_plugin_contract() {
        assert_eq!(signal_tag(SignalType::Logs), 0);
        assert_eq!(signal_tag(SignalType::Metrics), 1);
        assert_eq!(signal_tag(SignalType::Traces), 2);
    }

    #[test]
    fn rebuild_pdata_round_trips_signal() {
        let p = rebuild_pdata(SignalType::Logs, vec![1, 2, 3], Context::default());
        match p.payload_ref() {
            OtapPayload::OtlpBytes(OtlpProtoBytes::ExportLogsRequest(b)) => {
                assert_eq!(b.as_ref(), &[1, 2, 3]);
            }
            _ => panic!("unexpected payload variant"),
        }
    }

    #[test]
    fn payload_conversion_returns_signal() {
        let payload =
            OtapPayload::OtlpBytes(OtlpProtoBytes::ExportTracesRequest(Bytes::from_static(&[
                0x42,
            ])));
        let (sig, bytes) = payload_to_otlp_bytes(payload).unwrap();
        assert_eq!(sig, SignalType::Traces);
        assert_eq!(bytes, vec![0x42]);
    }

    // ----- end-to-end transform tests with a mock PluginRunner ----------

    use otap_df_config::transport_headers::{TransportHeader, TransportHeaders};
    use otap_df_plugin_api::{PLUGIN_API_VERSION, PluginFingerprint};
    use otap_df_plugin_host::PayloadKind as PK;
    use std::sync::Mutex;

    /// Test PluginRunner that records what it receives and returns a
    /// pre-canned response. Lets us assert that the host:
    ///   * forwards the OTLP-proto bytes verbatim,
    ///   * forwards the JSON config verbatim,
    ///   * tags the signal correctly,
    ///   * passes through the plugin's deadline.
    #[derive(Debug)]
    struct MockRunner {
        response: Result<(u32, Vec<u8>), String>,
        last_signal: Mutex<u32>,
        last_payload: Mutex<Vec<u8>>,
        last_config: Mutex<String>,
        last_timeout: Mutex<u64>,
    }

    impl MockRunner {
        fn new(response: Result<(u32, Vec<u8>), String>) -> Self {
            Self {
                response,
                last_signal: Mutex::new(0),
                last_payload: Mutex::new(Vec::new()),
                last_config: Mutex::new(String::new()),
                last_timeout: Mutex::new(0),
            }
        }
    }

    impl PluginRunner for MockRunner {
        fn process(
            &self,
            signal: u32,
            _payload_kind: PK,
            payload: &[u8],
            config_json: &str,
            timeout_ms: u64,
        ) -> Result<(u32, Vec<u8>), String> {
            *self.last_signal.lock().unwrap() = signal;
            *self.last_payload.lock().unwrap() = payload.to_vec();
            *self.last_config.lock().unwrap() = config_json.to_string();
            *self.last_timeout.lock().unwrap() = timeout_ms;
            self.response.clone()
        }
    }

    fn fp() -> PluginFingerprint {
        PluginFingerprint {
            component_urn: "urn:otel:test:proc".into(),
            plugin_version: "0.0.0".into(),
            artifact_sha256: "00".into(),
            plugin_api_version: PLUGIN_API_VERSION,
        }
    }

    fn cache_key() -> ComponentCacheKey {
        ComponentCacheKey {
            artifact_sha256: "00".into(),
            wasmtime_version: "0.0.0".into(),
            engine_config_fingerprint: "test".into(),
            target_triple: "x86_64-unknown-linux-gnu".into(),
            plugin_api_version: PLUGIN_API_VERSION,
        }
    }

    fn make_adapter(runner: Arc<dyn PluginRunner>) -> WasmProcessorAdapter {
        WasmProcessorAdapter {
            component_urn: Arc::from("urn:otel:test:proc"),
            fingerprint: fp(),
            cache_key: cache_key(),
            limits: Limits::default(),
            config_json: "{\"k\":\"v\"}".into(),
            runner,
            node_id: otap_df_engine::testing::test_node("test_proc".to_string()),
        }
    }

    /// Build a pdata with non-default Context (transport headers + a
    /// frame on the stack). PartialEq on `Context` lets us assert
    /// preservation.
    fn make_pdata_with_context(payload_bytes: Vec<u8>) -> (OtapPdata, Context) {
        let mut headers = TransportHeaders::with_capacity(2);
        headers.push(TransportHeader::text(
            "x-tenant-id",
            "x-tenant-id",
            "tenant-42",
        ));
        headers.push(TransportHeader::text(
            "x-request-id",
            "x-request-id",
            "req-abc",
        ));
        let mut ctx = Context::default();
        ctx.set_transport_headers(headers);
        let payload = OtapPayload::OtlpBytes(OtlpProtoBytes::ExportLogsRequest(Bytes::from(
            payload_bytes,
        )));
        let pdata = OtapPdata::new(ctx.clone(), payload);
        (pdata, ctx)
    }

    #[test]
    fn transform_preserves_context_and_forwards_inputs() {
        let runner = Arc::new(MockRunner::new(Ok((OK, vec![9, 9, 9]))));
        let adapter = make_adapter(runner.clone());

        let (pdata, original_ctx) = make_pdata_with_context(vec![1, 2, 3, 4]);
        let out = adapter
            .transform(pdata)
            .expect("transform succeeds")
            .expect("non-empty emit");

        // Inputs forwarded verbatim.
        assert_eq!(*runner.last_signal.lock().unwrap(), 0); // logs
        assert_eq!(*runner.last_payload.lock().unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(&*runner.last_config.lock().unwrap(), "{\"k\":\"v\"}");
        assert_eq!(
            *runner.last_timeout.lock().unwrap(),
            Limits::default().timeout_ms
        );

        // Plugin output payload is rebuilt under the original context.
        let (ctx, payload) = out.into_parts();
        assert_eq!(
            ctx, original_ctx,
            "Context (transport headers + frames) must survive plugin call",
        );
        match payload {
            OtapPayload::OtlpBytes(OtlpProtoBytes::ExportLogsRequest(b)) => {
                assert_eq!(b.as_ref(), &[9, 9, 9]);
            }
            _ => panic!("expected logs export payload"),
        }
    }

    #[test]
    fn transform_ok_empty_yields_no_emit() {
        let runner = Arc::new(MockRunner::new(Ok((OK, vec![]))));
        let adapter = make_adapter(runner);
        let (pdata, _) = make_pdata_with_context(vec![1]);
        assert!(adapter.transform(pdata).unwrap().is_none());
    }

    #[test]
    fn transform_drop_class_yields_no_emit() {
        let runner = Arc::new(MockRunner::new(Ok((DROP, vec![1, 2, 3]))));
        let adapter = make_adapter(runner);
        let (pdata, _) = make_pdata_with_context(vec![1]);
        assert!(adapter.transform(pdata).unwrap().is_none());
    }

    #[test]
    fn transform_retryable_class_returns_error() {
        let runner = Arc::new(MockRunner::new(Ok((RETRYABLE, vec![]))));
        let adapter = make_adapter(runner);
        let (pdata, _) = make_pdata_with_context(vec![1]);
        let err = adapter.transform(pdata).unwrap_err();
        assert!(matches!(err, Error::ProcessorError { .. }));
    }

    #[test]
    fn transform_permanent_class_returns_error() {
        let runner = Arc::new(MockRunner::new(Ok((PERMANENT, vec![]))));
        let adapter = make_adapter(runner);
        let (pdata, _) = make_pdata_with_context(vec![1]);
        let err = adapter.transform(pdata).unwrap_err();
        assert!(matches!(err, Error::ProcessorError { .. }));
    }

    #[test]
    fn transform_runner_error_returns_error() {
        let runner = Arc::new(MockRunner::new(Err("boom".into())));
        let adapter = make_adapter(runner);
        let (pdata, _) = make_pdata_with_context(vec![1]);
        let err = adapter.transform(pdata).unwrap_err();
        match err {
            Error::ProcessorError { error, .. } => assert!(error.contains("boom")),
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    // ----- exporter blocking-pool concurrency cap tests -----------------

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Semaphore;

    /// Runner that records (in-flight, peak in-flight) across calls and
    /// gates each call on a shared `AtomicBool` so tests can hold
    /// dispatches in flight while observing concurrency. Spinning on the
    /// flag inside the blocking pool keeps the runner tokio-runtime
    /// agnostic.
    struct GatedRunner {
        in_flight: AtomicUsize,
        peak: AtomicUsize,
        release: Arc<AtomicBool>,
    }

    impl std::fmt::Debug for GatedRunner {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("GatedRunner").finish_non_exhaustive()
        }
    }

    impl GatedRunner {
        fn new(release: Arc<AtomicBool>) -> Self {
            Self {
                in_flight: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                release,
            }
        }
    }

    impl PluginRunner for GatedRunner {
        fn process(
            &self,
            _signal: u32,
            _payload_kind: PK,
            _payload: &[u8],
            _config_json: &str,
            _timeout_ms: u64,
        ) -> Result<(u32, Vec<u8>), String> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = self.peak.fetch_max(now, Ordering::SeqCst);
            // Sleep-poll the release flag — runs inside spawn_blocking
            // so blocking the worker thread is fine.
            while !self.release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok((OK, Vec::new()))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dispatch_cap_limits_in_flight_calls() {
        // Cap = 2; spawn 5 concurrent dispatches; assert peak in-flight
        // never exceeds the cap. Each dispatch blocks in the runner
        // until we set the release flag.
        let permits = Arc::new(Semaphore::new(2));
        let release = Arc::new(AtomicBool::new(false));
        let runner = Arc::new(GatedRunner::new(Arc::clone(&release)));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let r: Arc<dyn PluginRunner> = runner.clone();
            let p = Arc::clone(&permits);
            handles.push(tokio::spawn(async move {
                WasmExporterAdapter::dispatch_one(r, p, 0, vec![], "{}".into(), 100).await
            }));
        }

        // Let the two scheduled dispatches reach the gate. Each runner
        // increments `in_flight` synchronously on entry; once we observe
        // 2 in flight we know the cap is being held and the remaining
        // dispatches are parked in `Semaphore::acquire_owned`.
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if runner.in_flight.load(Ordering::SeqCst) == 2 {
                break;
            }
        }
        assert_eq!(
            runner.in_flight.load(Ordering::SeqCst),
            2,
            "exactly cap=2 dispatches should be in flight before release"
        );
        assert!(
            runner.peak.load(Ordering::SeqCst) <= 2,
            "peak in-flight = {} must be <= cap=2",
            runner.peak.load(Ordering::SeqCst)
        );

        // Release all dispatches. Permits returned by completing
        // dispatches are immediately handed to parked waiters.
        release.store(true, Ordering::SeqCst);

        for h in handles {
            let res = h.await.expect("join").expect("dispatch ok");
            assert_eq!(res.0, OK);
        }

        assert!(
            runner.peak.load(Ordering::SeqCst) <= 2,
            "post-run: peak {} must still be <= cap=2",
            runner.peak.load(Ordering::SeqCst)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_returns_permit_acquire_error_when_semaphore_closed() {
        let permits = Arc::new(Semaphore::new(1));
        permits.close();
        let runner: Arc<dyn PluginRunner> = Arc::new(MockRunner::new(Ok((OK, vec![]))));

        let err = WasmExporterAdapter::dispatch_one(
            runner,
            Arc::clone(&permits),
            0,
            vec![],
            "{}".into(),
            100,
        )
        .await
        .expect_err("closed semaphore must surface as PermitAcquire error");
        assert!(matches!(err, DispatchError::PermitAcquire(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_releases_permit_after_completion() {
        // After a single dispatch finishes, the permit must be returned
        // to the pool so a subsequent dispatch can proceed.
        let permits = Arc::new(Semaphore::new(1));
        let runner: Arc<dyn PluginRunner> = Arc::new(MockRunner::new(Ok((OK, vec![1, 2]))));

        WasmExporterAdapter::dispatch_one(
            Arc::clone(&runner),
            Arc::clone(&permits),
            0,
            vec![],
            "{}".into(),
            100,
        )
        .await
        .map(|_| ())
        .expect("first dispatch ok");

        // If the permit had leaked, this acquire would block; we use
        // try_acquire to make the test deterministic.
        let permit = permits
            .try_acquire()
            .expect("permit must be available after dispatch returns");
        drop(permit);

        // And another dispatch should still succeed.
        WasmExporterAdapter::dispatch_one(runner, permits, 0, vec![], "{}".into(), 100)
            .await
            .map(|_| ())
            .expect("second dispatch ok");
    }
}
