// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Geneva Exporter for OTAP logs
//!
//! This exporter sends OTAP log data to Microsoft Geneva telemetry backend.
//! It converts OtapPdata to Geneva-compatible format and uses the geneva-uploader
//! crate for encoding and transmission.

use crate::OTAP_EXPORTER_FACTORIES;
use crate::metrics::ExporterPDataMetrics;
use crate::pdata::{OtapPayload, OtapPdata};
use async_trait::async_trait;
use geneva_uploader::client::{
    EncoderType, GenevaClient, GenevaClientConfig,
};
use linkme::distributed_slice;
use otap_df_config::experimental::SignalType;
use otap_df_config::node::NodeUserConfig;
use otap_df_engine::config::ExporterConfig;
use otap_df_engine::context::PipelineContext;
use otap_df_engine::control::{AckMsg, NodeControlMsg};
use otap_df_engine::error::Error;
use otap_df_engine::ConsumerEffectHandlerExtension;
use otap_df_engine::exporter::ExporterWrapper;
use otap_df_engine::local::exporter::{EffectHandler, Exporter};
use otap_df_engine::message::MessageChannel;
use otap_df_engine::message::Message;
use otap_df_engine::node::NodeId;
use otap_df_engine::terminal_state::TerminalState;
use otap_df_engine::ExporterFactory;
use otap_df_telemetry::metrics::MetricSet;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

/// The URN for the Geneva exporter
pub const GENEVA_EXPORTER_URN: &str = "urn:otel:geneva:exporter";

/// Configuration for the Geneva Exporter
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Geneva endpoint URL
    pub endpoint: String,
    /// Environment (e.g., "production", "staging")
    pub environment: String,
    /// Geneva account name
    pub account: String,
    /// Geneva namespace
    pub namespace: String,
    /// Azure region
    pub region: String,
    /// Config major version
    #[serde(default = "default_config_version")]
    pub config_major_version: u32,
    /// Tenant name
    pub tenant: String,
    /// Role name
    pub role_name: String,
    /// Role instance identifier
    pub role_instance: String,
    /// Authentication configuration
    pub auth: AuthConfig,
    /// Maximum concurrent uploads (default: 4)
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_uploads: usize,
    /// Maximum buffer size before forcing flush (default: 1000)
    #[serde(default = "default_buffer_size")]
    pub max_buffer_size: usize,
}

fn default_config_version() -> u32 {
    1
}

fn default_max_concurrent() -> usize {
    4
}

fn default_buffer_size() -> usize {
    1000
}

/// Authentication configuration
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuthConfig {
    /// Certificate-based authentication (PKCS#12 format)
    /// 
    /// Requires a PKCS#12 (.p12) file containing both certificate and private key.
    /// To convert from PEM format:
    /// ```bash
    /// openssl pkcs12 -export -in cert.pem -inkey key.pem -out client.p12
    /// ```
    Certificate {
        /// Path to PKCS#12 (.p12) certificate file
        path: String,
        /// Password to decrypt the PKCS#12 file
        password: String,
    },
    /// System-assigned managed identity
    SystemManagedIdentity {
        /// MSI resource identifier
        msi_resource: String
    },
    /// User-assigned managed identity (by client ID)
    UserManagedIdentity {
        /// Client ID of the managed identity
        client_id: String,
        /// MSI resource identifier
        msi_resource: String,
    },
    /// Workload identity (Kubernetes)
    WorkloadIdentity {
        /// MSI resource identifier
        msi_resource: String,
    },
}

/// Geneva exporter that sends OTAP data to Geneva backend
pub struct GenevaExporter {
    geneva_client: Arc<GenevaClient>,
    config: Config,
    pdata_metrics: MetricSet<ExporterPDataMetrics>,
}

/// Declare the Geneva Exporter as a local exporter factory
#[allow(unsafe_code)]
#[distributed_slice(OTAP_EXPORTER_FACTORIES)]
pub static GENEVA_EXPORTER: ExporterFactory<OtapPdata> = ExporterFactory {
    name: GENEVA_EXPORTER_URN,
    create: |pipeline: PipelineContext,
             node: NodeId,
             node_config: Arc<NodeUserConfig>,
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
    /// Create a new Geneva exporter from configuration
    pub fn from_config(
        pipeline_ctx: PipelineContext,
        config: &serde_json::Value,
    ) -> Result<Self, otap_df_config::error::Error> {
        let pdata_metrics = pipeline_ctx.register_metrics::<ExporterPDataMetrics>();

        let config: Config = serde_json::from_value(config.clone()).map_err(|e| {
            otap_df_config::error::Error::InvalidUserConfig {
                error: e.to_string(),
            }
        })?;

        // Convert auth config to geneva-uploader format
        let auth_method = match &config.auth {
            AuthConfig::Certificate {
                path,
                password,
            } => geneva_uploader::AuthMethod::Certificate {
                path: path.clone().into(),
                password: password.clone(),
            },
            AuthConfig::SystemManagedIdentity { msi_resource: _ } => {
                geneva_uploader::AuthMethod::SystemManagedIdentity
            }
            AuthConfig::UserManagedIdentity {
                client_id,
                msi_resource: _,
            } => geneva_uploader::AuthMethod::UserManagedIdentity {
                client_id: client_id.clone(),
            },
            AuthConfig::WorkloadIdentity {
                msi_resource,
            } => geneva_uploader::AuthMethod::WorkloadIdentity {
                resource: msi_resource.clone(),
            },
        };

        let msi_resource = match &config.auth {
            AuthConfig::SystemManagedIdentity { msi_resource }
            | AuthConfig::UserManagedIdentity { msi_resource, .. }
            | AuthConfig::WorkloadIdentity { msi_resource, .. } => Some(msi_resource.clone()),
            _ => None,
        };

        // Create Geneva client
        let geneva_config = GenevaClientConfig {
            endpoint: config.endpoint.clone(),
            environment: config.environment.clone(),
            account: config.account.clone(),
            namespace: config.namespace.clone(),
            region: config.region.clone(),
            config_major_version: config.config_major_version,
            auth_method,
            tenant: config.tenant.clone(),
            role_name: config.role_name.clone(),
            role_instance: config.role_instance.clone(),
            msi_resource,
            encoder_type: EncoderType::Otap, // Use OTAP encoder
        };

        let geneva_client = GenevaClient::new(geneva_config).map_err(|e| {
            otap_df_config::error::Error::InvalidUserConfig {
                error: format!("Failed to create Geneva client: {}", e),
            }
        })?;

        Ok(Self {
            geneva_client: Arc::new(geneva_client),
            config,
            pdata_metrics,
        })
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
                "Geneva exporter starting: endpoint={}, namespace={}",
                self.config.endpoint, self.config.namespace
            ))
            .await;

        let _exporter_id = effect_handler.exporter_id();
        let timer_cancel_handle = effect_handler
            .start_periodic_telemetry(Duration::from_secs(1))
            .await?;

        // No buffering needed - we encode directly from Arrow RecordBatches

        loop {
            match msg_chan.recv().await? {
                Message::Control(NodeControlMsg::Shutdown { deadline, .. }) => {
                    // No flush needed - we encode immediately
                    effect_handler
                        .info("Geneva exporter shutting down")
                        .await;

                    _ = timer_cancel_handle.cancel().await;
                    return Ok(TerminalState::new(deadline, [self.pdata_metrics]));
                }
                Message::Control(NodeControlMsg::CollectTelemetry {
                    mut metrics_reporter,
                }) => {
                    _ = metrics_reporter.report(&mut self.pdata_metrics);
                }
                Message::PData(mut pdata) => {
                    let signal_type = pdata.signal_type();

                    effect_handler
                        .info(&format!("✅ Geneva exporter received PData: {:?}", signal_type))
                        .await;

                    // Only handle logs
                    if signal_type != SignalType::Logs {
                        effect_handler
                            .info(&format!(
                                "⚠️  Geneva exporter only supports logs, got: {:?}",
                                signal_type
                            ))
                            .await;
                        continue;
                    }

                    self.pdata_metrics.inc_consumed(signal_type);

                    let payload = pdata.take_payload();
                    let _ = effect_handler.notify_ack(AckMsg::new(pdata)).await?;

                    match payload {
                        OtapPayload::OtapArrowRecords(otap_batch) => {
                            use otel_arrow_rust::proto::opentelemetry::arrow::v1::ArrowPayloadType;

                            let num_rows = otap_batch.get(ArrowPayloadType::Logs).map(|b| b.num_rows()).unwrap_or(0);
                            effect_handler
                                .info(&format!("📦 Processing OTAP Arrow batch with {} rows", num_rows))
                                .await;

                            // Encode directly from Arrow RecordBatches (zero-copy!)
                            let logs_batch = otap_batch.get(ArrowPayloadType::Logs);
                            let log_attrs_batch = otap_batch.get(ArrowPayloadType::LogAttrs);
                            let resource_attrs_batch = otap_batch.get(ArrowPayloadType::ResourceAttrs);

                            if let Some(logs_batch) = logs_batch {
                                effect_handler
                                    .info(&format!("✅ Encoding {} log records directly from Arrow (zero-copy)", num_rows))
                                    .await;

                                match self.geneva_client.encode_and_compress_arrow_logs(
                                    logs_batch,
                                    log_attrs_batch,
                                    resource_attrs_batch,
                                ) {
                                            Ok(batches) => {
                                                effect_handler
                                                    .info(&format!("✅ Encoded {} batches", batches.len()))
                                                    .await;

                                                // Upload batches
                                                for (idx, batch) in batches.iter().enumerate() {
                                                    effect_handler
                                                        .info(&format!("📤 Uploading batch {}/{} (size: {} bytes)", idx + 1, batches.len(), batch.data.len()))
                                                        .await;

                                                    if let Err(e) = self
                                                        .geneva_client
                                                        .upload_batch(batch)
                                                        .await
                                                    {
                                                        self.pdata_metrics.logs_failed.inc();
                                                        effect_handler
                                                            .info(&format!(
                                                                "❌ Failed to upload batch {}: {}",
                                                                idx + 1, e
                                                            ))
                                                            .await;
                                                    } else {
                                                        self.pdata_metrics.logs_exported.inc();
                                                        effect_handler
                                                            .info(&format!("✅ Successfully uploaded batch {}", idx + 1))
                                                            .await;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                self.pdata_metrics.logs_failed.inc();
                                                effect_handler
                                                    .info(&format!("❌ Failed to encode logs: {}", e))
                                                    .await;
                                            }
                                        }
                            } else {
                                effect_handler
                                    .info("⚠️  No logs batch found in OTAP Arrow records")
                                    .await;
                            }
                        }
                        _ => {
                            effect_handler
                                .info("⚠️  Geneva exporter only supports OTAP Arrow records")
                                .await;
                        }
                    }
                }
                _ => {
                    // Ignore other messages
                }
            }
        }
    }
}
