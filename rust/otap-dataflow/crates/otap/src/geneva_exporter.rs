// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Geneva exporter for OTAP data pipelines.
//!
//! Converts incoming OTLP/OTAP payloads for logs and traces into Geneva uploader
//! batches and uploads them via the shared Geneva uploader crate.

use crate::metrics::ExporterPDataMetrics;
use crate::pdata::{Context, OtapPayload, OtapPdata, OtlpProtoBytes};
use crate::OTAP_EXPORTER_FACTORIES;
use async_trait::async_trait;
use geneva_exporter::{BatchRetryConfig, GenevaExporterConfig};
use geneva_uploader::client::{EncodedBatch, GenevaClient};
use linkme::distributed_slice;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use otap_df_config::experimental::SignalType;
use otap_df_config::node::NodeUserConfig;
use otap_df_engine::config::ExporterConfig;
use otap_df_engine::context::PipelineContext;
use otap_df_engine::control::{AckMsg, NackMsg, NodeControlMsg};
use otap_df_engine::error::{Error, ExporterErrorKind};
use otap_df_engine::exporter::ExporterWrapper;
use otap_df_engine::local::exporter::{EffectHandler, Exporter};
use otap_df_engine::message::{Message, MessageChannel};
use otap_df_engine::node::NodeId;
use otap_df_engine::terminal_state::TerminalState;
use otap_df_engine::{ConsumerEffectHandlerExtension, ExporterFactory};
use otap_df_telemetry::metrics::MetricSet;
use prost::Message;
use serde::Deserialize;
use std::cmp;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, warn};

/// URN exposed to the OTAP dataflow configuration.
pub const GENEVA_EXPORTER_URN: &str = "urn:otel:geneva:exporter";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(flatten)]
    geneva: GenevaExporterConfig,
}

/// Exporter wiring Geneva uploader into OTAP pipelines.
pub struct GenevaExporter {
    client: GenevaClient,
    config: GenevaExporterConfig,
    pdata_metrics: MetricSet<ExporterPDataMetrics>,
}

#[allow(unsafe_code)]
#[distributed_slice(OTAP_EXPORTER_FACTORIES)]
pub static GENEVA_EXPORTER: ExporterFactory<OtapPdata> = ExporterFactory {
    name: GENEVA_EXPORTER_URN,
    create: |pipeline: PipelineContext,
             node: NodeId,
             node_config: std::sync::Arc<NodeUserConfig>,
             exporter_config: &ExporterConfig| {
        Ok(ExporterWrapper::local(
            GenevaExporter::from_config(pipeline, &node_config.config)?,
            node,
            node_config,
            exporter_config,
        ))
    },
};

impl GenevaExporter {
    /// Instantiate from JSON configuration.
    pub fn from_config(
        pipeline_ctx: PipelineContext,
        config: &serde_json::Value,
    ) -> Result<Self, otap_df_config::error::Error> {
        let cfg: Config = serde_json::from_value(config.clone()).map_err(|e| {
            otap_df_config::error::Error::InvalidUserConfig {
                error: e.to_string(),
            }
        })?;

        let geneva_cfg = cfg.geneva.clone();
        let client = geneva_cfg.build_client().map_err(|e| {
            otap_df_config::error::Error::InvalidUserConfig {
                error: format!("failed constructing Geneva client: {e}"),
            }
        })?;

        let pdata_metrics = pipeline_ctx.register_metrics::<ExporterPDataMetrics>();

        Ok(Self {
            client,
            config: geneva_cfg,
            pdata_metrics,
        })
    }

    /// Process a logs signal.
    async fn handle_logs(
        &self,
        context: Context,
        payload: OtapPayload,
        effect_handler: &EffectHandler<OtapPdata>,
    ) -> Result<(), Error> {
        let ack_payload = payload.clone();
        let exporter_id = effect_handler.exporter_id();

        let bytes = self.extract_otlp_bytes(&payload, SignalType::Logs)?;
        let request = ExportLogsServiceRequest::decode(bytes.as_slice()).map_err(|e| {
            let error_msg = format!("failed to decode ExportLogsServiceRequest: {e}");
            let error = Error::ExporterError {
                exporter: exporter_id.clone(),
                kind: ExporterErrorKind::Other,
                error: error_msg.clone(),
                source_detail: String::new(),
            };
            _ = effect_handler.notify_nack(NackMsg::new(
                &error_msg,
                OtapPdata::new(context, ack_payload),
            ));
            error
        })?;

        if request.resource_logs.is_empty() {
            return effect_handler
                .notify_ack(AckMsg::new(OtapPdata::new(context, ack_payload)))
                .await;
        }

        let batches = self
            .client
            .encode_and_compress_logs(&request.resource_logs)
            .map_err(|e| {
                let error_msg = format!("log compression failed: {e}");
                let error = Error::ExporterError {
                    exporter: exporter_id.clone(),
                    kind: ExporterErrorKind::Other,
                    error: error_msg.clone(),
                    source_detail: String::new(),
                };
                _ = effect_handler.notify_nack(NackMsg::new(
                    &error_msg,
                    OtapPdata::new(context, ack_payload),
                ));
                error
            })?;

        if let Err(error) = self.upload_batches(&batches).await {
            let error_msg = format!("Geneva upload failed: {error}");
            let error = Error::ExporterError {
                exporter: exporter_id.clone(),
                kind: ExporterErrorKind::Transport,
                error: error_msg.clone(),
                source_detail: String::new(),
            };
            _ = effect_handler.notify_nack(NackMsg::new(
                &error_msg,
                OtapPdata::new(context, ack_payload),
            ));
            return Err(error);
        }

        effect_handler
            .notify_ack(AckMsg::new(OtapPdata::new(context, ack_payload)))
            .await
    }

    /// Process a traces signal.
    async fn handle_traces(
        &self,
        context: Context,
        payload: OtapPayload,
        effect_handler: &EffectHandler<OtapPdata>,
    ) -> Result<(), Error> {
        let ack_payload = payload.clone();
        let exporter_id = effect_handler.exporter_id();

        let bytes = self.extract_otlp_bytes(&payload, SignalType::Traces)?;
        let request = ExportTraceServiceRequest::decode(bytes.as_slice()).map_err(|e| {
            let error_msg = format!("failed to decode ExportTraceServiceRequest: {e}");
            let error = Error::ExporterError {
                exporter: exporter_id.clone(),
                kind: ExporterErrorKind::Other,
                error: error_msg.clone(),
                source_detail: String::new(),
            };
            _ = effect_handler.notify_nack(NackMsg::new(
                &error_msg,
                OtapPdata::new(context, ack_payload),
            ));
            error
        })?;

        if request.resource_spans.is_empty() {
            return effect_handler
                .notify_ack(AckMsg::new(OtapPdata::new(context, ack_payload)))
                .await;
        }

        let batches = self
            .client
            .encode_and_compress_spans(&request.resource_spans)
            .map_err(|e| {
                let error_msg = format!("trace compression failed: {e}");
                let error = Error::ExporterError {
                    exporter: exporter_id.clone(),
                    kind: ExporterErrorKind::Other,
                    error: error_msg.clone(),
                    source_detail: String::new(),
                };
                _ = effect_handler.notify_nack(NackMsg::new(
                    &error_msg,
                    OtapPdata::new(context, ack_payload),
                ));
                error
            })?;

        if let Err(error) = self.upload_batches(&batches).await {
            let error_msg = format!("Geneva upload failed: {error}");
            let error = Error::ExporterError {
                exporter: exporter_id.clone(),
                kind: ExporterErrorKind::Transport,
                error: error_msg.clone(),
                source_detail: String::new(),
            };
            _ = effect_handler.notify_nack(NackMsg::new(
                &error_msg,
                OtapPdata::new(context, ack_payload),
            ));
            return Err(error);
        }

        effect_handler
            .notify_ack(AckMsg::new(OtapPdata::new(context, ack_payload)))
            .await
    }

    fn extract_otlp_bytes(
        &self,
        payload: &OtapPayload,
        signal: SignalType,
    ) -> Result<Vec<u8>, Error> {
        let otlp = match payload {
            OtapPayload::OtlpBytes(bytes) => bytes.clone(),
            _ => OtlpProtoBytes::try_from(payload.clone()).map_err(Error::from)?,
        };

        match (signal, otlp) {
            (SignalType::Logs, OtlpProtoBytes::ExportLogsRequest(bytes)) => Ok(bytes),
            (SignalType::Traces, OtlpProtoBytes::ExportTracesRequest(bytes)) => Ok(bytes),
            (_, other) => Err(Error::ExporterError {
                exporter: String::from(GENEVA_EXPORTER_URN).into(),
                kind: ExporterErrorKind::Other,
                error: format!(
                    "unsupported or mismatched OTLP payload variant: {:?}",
                    other.signal_type()
                ),
                source_detail: String::new(),
            }),
        }
    }

    async fn upload_batches(&self, batches: &[EncodedBatch]) -> Result<(), String> {
        for (idx, batch) in batches.iter().enumerate() {
            if batch.data.is_empty() {
                debug!(
                    event_name = %batch.event_name,
                    "Skipping empty Geneva batch at index {idx}"
                );
                continue;
            }

            if let Err(err) = self.upload_with_retry(batch).await {
                return Err(format!("batch #{idx} upload error: {err}"));
            }
        }
        Ok(())
    }

    async fn upload_with_retry(&self, batch: &EncodedBatch) -> Result<(), String> {
        let retry_cfg: &BatchRetryConfig = &self.config.batch_retry;
        if !retry_cfg.enabled || retry_cfg.max_retries == 0 {
            return self.client.upload_batch(batch).await;
        }

        let max_attempts = cmp::max(1, retry_cfg.max_retries as usize);
        let mut attempt = 0;
        let mut delay = retry_cfg.initial_interval;

        loop {
            attempt += 1;
            match self.client.upload_batch(batch).await {
                Ok(()) => return Ok(()),
                Err(err) if attempt >= max_attempts => return Err(err),
                Err(err) => {
                    warn!(
                        event_name = %batch.event_name,
                        attempt,
                        max_attempts,
                        error = %err,
                        "Geneva upload attempt failed, retrying"
                    );

                    if delay.is_zero() {
                        continue;
                    }

                    sleep(delay).await;
                    let mut next_delay = delay.mul_f64(retry_cfg.multiplier);
                    if retry_cfg.max_interval > Duration::ZERO {
                        next_delay = cmp::min(next_delay, retry_cfg.max_interval);
                    }
                    delay = next_delay;
                }
            }
        }
    }
}

#[async_trait(?Send)]
impl Exporter<OtapPdata> for GenevaExporter {
    async fn start(
        mut self: Box<Self>,
        mut msg_chan: MessageChannel<OtapPdata>,
        effect_handler: EffectHandler<OtapPdata>,
    ) -> Result<TerminalState, Error> {
        effect_handler
            .info(&format!(
                "Exporting Geneva traffic to namespace `{}` (account: `{}`)",
                self.config.namespace, self.config.account
            ))
            .await;

        loop {
            match msg_chan.recv().await? {
                Message::Control(NodeControlMsg::Shutdown { deadline, .. }) => {
                    return Ok(TerminalState::new(deadline, [self.pdata_metrics]));
                }
                Message::Control(NodeControlMsg::CollectTelemetry {
                    mut metrics_reporter,
                }) => {
                    _ = metrics_reporter.report(&mut self.pdata_metrics);
                }
                Message::PData(pdata) => {
                    let signal_type = pdata.signal_type();
                    let (context, payload) = pdata.into_parts();
                    self.pdata_metrics.inc_consumed(signal_type);

                    match signal_type {
                        SignalType::Logs => {
                            match self.handle_logs(context, payload, &effect_handler).await {
                                Ok(_) => self.pdata_metrics.logs_exported.inc(),
                                Err(_) => self.pdata_metrics.logs_failed.inc(),
                            }
                        }
                        SignalType::Traces => {
                            match self.handle_traces(context, payload, &effect_handler).await {
                                Ok(_) => self.pdata_metrics.traces_exported.inc(),
                                Err(_) => self.pdata_metrics.traces_failed.inc(),
                            }
                        }
                        SignalType::Metrics => {
                            self.pdata_metrics.metrics_failed.inc();
                            let error_msg =
                                "Geneva exporter does not support metrics signals yet".to_string();
                            _ = effect_handler
                                .notify_nack(NackMsg::new(
                                    &error_msg,
                                    OtapPdata::new(context, payload),
                                ))
                                .await?;
                        }
                    }
                }
                _ => {
                    // ignore other messages
                }
            }
        }
    }
}
