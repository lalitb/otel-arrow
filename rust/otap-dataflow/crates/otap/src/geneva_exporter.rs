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
use arrow::array::Array;
use async_trait::async_trait;
use geneva_uploader::client::{
    AttributeValue, EncoderType, GenevaClient, GenevaClientConfig, LogAttribute, LogBody,
    LogRecordData,
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
use otel_arrow_rust::otap::OtapArrowRecords;
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

    /// Extract attributes from LogAttrs batch and attach them to log records
    fn extract_and_attach_attributes(
        log_records: &mut [LogRecordData],
        attrs_batch: &arrow::array::RecordBatch,
        _attr_type: &str,  // "log" or "resource"
    ) -> Result<(), String> {
        use arrow::array::DictionaryArray;

        let num_attr_rows = attrs_batch.num_rows();

        // Get columns from attributes batch
        let parent_id_col = attrs_batch
            .column_by_name("parent_id")
            .ok_or("parent_id column not found")?
            .as_any()
            .downcast_ref::<arrow::array::UInt16Array>()
            .ok_or("parent_id is not UInt16Array")?;

        let key_col = attrs_batch
            .column_by_name("key")
            .ok_or("key column not found")?;

        let type_col = attrs_batch
            .column_by_name("type")
            .ok_or("type column not found")?
            .as_any()
            .downcast_ref::<arrow::array::UInt8Array>()
            .ok_or("type is not UInt8Array")?;

        let str_col = attrs_batch.column_by_name("str");
        let int_col = attrs_batch.column_by_name("int");

        // Process each attribute row
        for attr_idx in 0..num_attr_rows {
            if parent_id_col.is_null(attr_idx) {
                continue;
            }

            let parent_id = parent_id_col.value(attr_idx) as usize;

            // parent_id corresponds to the log record index (0-based)
            if parent_id >= log_records.len() {
                continue;
            }

            // Extract key (from Dictionary)
            let key = key_col
                .as_any()
                .downcast_ref::<DictionaryArray<arrow::datatypes::UInt8Type>>()
                .and_then(|dict_arr| {
                    if dict_arr.is_null(attr_idx) {
                        None
                    } else {
                        let key_idx = dict_arr.keys().value(attr_idx);
                        dict_arr
                            .values()
                            .as_any()
                            .downcast_ref::<arrow::array::StringArray>()
                            .map(|values| values.value(key_idx as usize).to_string())
                    }
                });

            if key.is_none() {
                continue;
            }
            let key = key.unwrap();

            // Extract value based on type
            let attr_type = type_col.value(attr_idx);
            let value = match attr_type {
                // Type 1 = string
                1 => {
                    str_col
                        .and_then(|col| {
                            col.as_any()
                                .downcast_ref::<DictionaryArray<arrow::datatypes::UInt16Type>>()
                        })
                        .and_then(|dict_arr| {
                            if dict_arr.is_null(attr_idx) {
                                None
                            } else {
                                let val_idx = dict_arr.keys().value(attr_idx);
                                dict_arr
                                    .values()
                                    .as_any()
                                    .downcast_ref::<arrow::array::StringArray>()
                                    .map(|values| {
                                        AttributeValue::String(
                                            values.value(val_idx as usize).to_string(),
                                        )
                                    })
                            }
                        })
                }
                // Type 2 = int
                2 => {
                    int_col
                        .and_then(|col| {
                            col.as_any()
                                .downcast_ref::<DictionaryArray<arrow::datatypes::UInt16Type>>()
                        })
                        .and_then(|dict_arr| {
                            if dict_arr.is_null(attr_idx) {
                                None
                            } else {
                                let val_idx = dict_arr.keys().value(attr_idx);
                                dict_arr
                                    .values()
                                    .as_any()
                                    .downcast_ref::<arrow::array::Int64Array>()
                                    .map(|values| {
                                        AttributeValue::Int(values.value(val_idx as usize))
                                    })
                            }
                        })
                }
                _ => None, // Other types not handled yet
            };

            if let Some(value) = value {
                log_records[parent_id].attributes.push(LogAttribute { key, value });
            }
        }

        Ok(())
    }

    /// Extract resource attributes and attach them to log records based on resource IDs
    fn extract_and_attach_resource_attributes(
        log_records: &mut [LogRecordData],
        res_attrs_batch: &arrow::array::RecordBatch,
        resource_ids: &[Option<u16>],
    ) -> Result<(), String> {
        use arrow::array::DictionaryArray;
        use std::collections::HashMap;

        // First, build a map of resource_id -> Vec<(key, value)>
        let mut resource_attrs_map: HashMap<u16, Vec<(String, AttributeValue)>> = HashMap::new();

        let num_attr_rows = res_attrs_batch.num_rows();

        // Get columns
        let parent_id_col = res_attrs_batch
            .column_by_name("parent_id")
            .ok_or("parent_id column not found in ResourceAttrs")?
            .as_any()
            .downcast_ref::<arrow::array::UInt16Array>()
            .ok_or("parent_id is not UInt16Array")?;

        let key_col = res_attrs_batch
            .column_by_name("key")
            .ok_or("key column not found in ResourceAttrs")?;

        let type_col = res_attrs_batch
            .column_by_name("type")
            .ok_or("type column not found in ResourceAttrs")?
            .as_any()
            .downcast_ref::<arrow::array::UInt8Array>()
            .ok_or("type is not UInt8Array")?;

        let str_col = res_attrs_batch.column_by_name("str");

        // Extract all resource attributes into the map
        for attr_idx in 0..num_attr_rows {
            if parent_id_col.is_null(attr_idx) {
                continue;
            }

            let resource_id = parent_id_col.value(attr_idx);

            // Extract key
            let key = key_col
                .as_any()
                .downcast_ref::<DictionaryArray<arrow::datatypes::UInt8Type>>()
                .and_then(|dict_arr| {
                    if dict_arr.is_null(attr_idx) {
                        None
                    } else {
                        let key_idx = dict_arr.keys().value(attr_idx);
                        dict_arr
                            .values()
                            .as_any()
                            .downcast_ref::<arrow::array::StringArray>()
                            .map(|values| values.value(key_idx as usize).to_string())
                    }
                });

            if key.is_none() {
                continue;
            }
            let key = key.unwrap();

            // Extract value (for now, only handling string type = 1)
            let attr_type = type_col.value(attr_idx);
            let value = if attr_type == 1 {
                // String
                str_col
                    .and_then(|col| {
                        col.as_any()
                            .downcast_ref::<DictionaryArray<arrow::datatypes::UInt16Type>>()
                    })
                    .and_then(|dict_arr| {
                        if dict_arr.is_null(attr_idx) {
                            None
                        } else {
                            let val_idx = dict_arr.keys().value(attr_idx);
                            dict_arr
                                .values()
                                .as_any()
                                .downcast_ref::<arrow::array::StringArray>()
                                .map(|values| {
                                    AttributeValue::String(values.value(val_idx as usize).to_string())
                                })
                        }
                    })
            } else {
                None
            };

            if let Some(value) = value {
                resource_attrs_map
                    .entry(resource_id)
                    .or_insert_with(Vec::new)
                    .push((key, value));
            }
        }

        // Now attach resource attributes to each log record based on its resource_id
        for (log_idx, resource_id_opt) in resource_ids.iter().enumerate() {
            if let Some(resource_id) = resource_id_opt {
                if let Some(attrs) = resource_attrs_map.get(resource_id) {
                    for (key, value) in attrs {
                        log_records[log_idx].attributes.push(LogAttribute {
                            key: key.clone(),
                            value: value.clone(),
                        });
                    }
                }
            }
        }

        Ok(())
    }

    /// Convert OTAP Arrow records to LogRecordData
    fn convert_otap_to_log_records(
        otap_batch: &OtapArrowRecords,
    ) -> Result<Vec<LogRecordData>, String> {
        let mut log_records = Vec::new();

        // Get the logs record batch from OTAP
        let arrow_payload_type = otel_arrow_rust::proto::opentelemetry::arrow::v1::ArrowPayloadType::Logs;
        let record_batch = otap_batch
            .get(arrow_payload_type)
            .ok_or("No logs record batch in OTAP data")?;

        let num_rows = record_batch.num_rows();

        // Get the log attributes record batch (if present)
        let log_attrs_batch = otap_batch.get(otel_arrow_rust::proto::opentelemetry::arrow::v1::ArrowPayloadType::LogAttrs);

        // Debug: Check if log attributes exist
        if let Some(attrs_batch) = log_attrs_batch {
            eprintln!("🔍 DEBUG LogAttrs Batch:");
            eprintln!("  Rows: {}", attrs_batch.num_rows());
            eprintln!("  Columns:");
            for field in attrs_batch.schema().fields() {
                eprintln!("    - {}: {:?}", field.name(), field.data_type());
            }
        } else {
            eprintln!("🔍 DEBUG: No LogAttrs batch found");
        }

        // Debug: Check for ResourceAttrs batch
        let resource_attrs_batch = otap_batch.get(otel_arrow_rust::proto::opentelemetry::arrow::v1::ArrowPayloadType::ResourceAttrs);
        if let Some(res_attrs_batch) = resource_attrs_batch {
            eprintln!("🔍 DEBUG ResourceAttrs Batch:");
            eprintln!("  Rows: {}", res_attrs_batch.num_rows());
            eprintln!("  Columns:");
            for field in res_attrs_batch.schema().fields() {
                eprintln!("    - {}: {:?}", field.name(), field.data_type());
            }
        }

        // Helper to get column by name
        let get_column = |name: &str| -> Result<&dyn Array, String> {
            record_batch
                .column_by_name(name)
                .map(|col| col.as_ref())
                .ok_or_else(|| format!("Column '{}' not found", name))
        };

        // Debug: Print all available columns and their types
        eprintln!("🔍 DEBUG Arrow Batch Columns:");
        for field in record_batch.schema().fields() {
            eprintln!("  - {}: {:?}", field.name(), field.data_type());
        }

        // Extract common columns (with error handling for missing columns)
        // Note: time_unix_nano is stored as TimestampNanosecondArray, not UInt64Array
        let time_unix_nano = get_column("time_unix_nano").ok().and_then(|col| {
            col.as_any()
                .downcast_ref::<arrow::array::TimestampNanosecondArray>()
        });

        let observed_time_unix_nano = get_column("observed_time_unix_nano")
            .ok()
            .and_then(|col| {
                col.as_any()
                    .downcast_ref::<arrow::array::TimestampNanosecondArray>()
            });

        // severity_number is Dictionary(UInt8, Int32)
        let severity_number = get_column("severity_number").ok();

        // severity_text is Dictionary(UInt8, Utf8)
        let severity_text = get_column("severity_text").ok();

        // body is Struct with "type" and "str" fields
        let body_col = get_column("body").ok();

        let trace_id = get_column("trace_id")
            .ok()
            .and_then(|col| col.as_any().downcast_ref::<arrow::array::BinaryArray>());

        let span_id = get_column("span_id")
            .ok()
            .and_then(|col| col.as_any().downcast_ref::<arrow::array::BinaryArray>());

        let flags = get_column("flags")
            .ok()
            .and_then(|col| col.as_any().downcast_ref::<arrow::array::UInt32Array>());

        // Helper to extract value from Dictionary(UInt8, Int32)
        let get_severity_number = |arr: &dyn Array, idx: usize| -> Option<i32> {
            use arrow::array::DictionaryArray;
            arr.as_any()
                .downcast_ref::<DictionaryArray<arrow::datatypes::UInt8Type>>()
                .and_then(|dict_arr| {
                    if dict_arr.is_null(idx) {
                        None
                    } else {
                        let key = dict_arr.keys().value(idx);
                        dict_arr.values()
                            .as_any()
                            .downcast_ref::<arrow::array::Int32Array>()
                            .map(|values| values.value(key as usize))
                    }
                })
        };

        // Helper to extract value from Dictionary(UInt8, Utf8)
        let get_severity_text = |arr: &dyn Array, idx: usize| -> Option<String> {
            use arrow::array::DictionaryArray;
            arr.as_any()
                .downcast_ref::<DictionaryArray<arrow::datatypes::UInt8Type>>()
                .and_then(|dict_arr| {
                    if dict_arr.is_null(idx) {
                        None
                    } else {
                        let key = dict_arr.keys().value(idx);
                        dict_arr.values()
                            .as_any()
                            .downcast_ref::<arrow::array::StringArray>()
                            .map(|values| values.value(key as usize).to_string())
                    }
                })
        };

        // Helper to extract body from Struct
        let get_body = |arr: &dyn Array, idx: usize| -> Option<LogBody> {
            use arrow::array::StructArray;
            arr.as_any()
                .downcast_ref::<StructArray>()
                .and_then(|struct_arr| {
                    if struct_arr.is_null(idx) {
                        None
                    } else {
                        // Get "str" field from the struct
                        struct_arr.column_by_name("str")
                            .and_then(|col| {
                                col.as_any()
                                    .downcast_ref::<arrow::array::DictionaryArray<arrow::datatypes::UInt16Type>>()
                            })
                            .and_then(|dict_arr| {
                                if dict_arr.is_null(idx) {
                                    None
                                } else {
                                    let key = dict_arr.keys().value(idx);
                                    dict_arr.values()
                                        .as_any()
                                        .downcast_ref::<arrow::array::StringArray>()
                                        .map(|values| LogBody::String(values.value(key as usize).to_string()))
                                }
                            })
                    }
                })
        };

        // Convert each row
        for row_idx in 0..num_rows {
            let log_record = LogRecordData {
                time_unix_nano: time_unix_nano.and_then(|arr| {
                    if arr.is_null(row_idx) {
                        None
                    } else {
                        // TimestampNanosecondArray.value() returns i64, cast to u64
                        Some(arr.value(row_idx) as u64)
                    }
                }),
                observed_time_unix_nano: observed_time_unix_nano.and_then(|arr| {
                    if arr.is_null(row_idx) {
                        None
                    } else {
                        // TimestampNanosecondArray.value() returns i64, cast to u64
                        Some(arr.value(row_idx) as u64)
                    }
                }),
                severity_number: severity_number.and_then(|arr| get_severity_number(arr, row_idx)),
                severity_text: severity_text.and_then(|arr| get_severity_text(arr, row_idx)),
                body: body_col.and_then(|arr| get_body(arr, row_idx)),
                attributes: Vec::new(), // Will be populated after extracting all log IDs
                trace_id: trace_id
                    .and_then(|arr| {
                        if arr.is_null(row_idx) {
                            None
                        } else {
                            Some(arr.value(row_idx).to_vec())
                        }
                    })
                    .unwrap_or_default(),
                span_id: span_id
                    .and_then(|arr| {
                        if arr.is_null(row_idx) {
                            None
                        } else {
                            Some(arr.value(row_idx).to_vec())
                        }
                    })
                    .unwrap_or_default(),
                flags: flags.and_then(|arr| {
                    if arr.is_null(row_idx) {
                        None
                    } else {
                        Some(arr.value(row_idx))
                    }
                }),
                event_name: None, // TODO: Extract from attributes or use default
            };

            log_records.push(log_record);
        }

        // Extract log attributes from LogAttrs batch and map them to log records
        if let Some(attrs_batch) = log_attrs_batch {
            Self::extract_and_attach_attributes(&mut log_records, attrs_batch, "log")?;
        }

        // Extract resource attributes from ResourceAttrs batch
        // First, we need to extract the resource IDs from each log record
        let resource_col = get_column("resource").ok();
        let resource_ids: Vec<Option<u16>> = if let Some(res_col) = resource_col {
            (0..num_rows)
                .map(|row_idx| {
                    res_col
                        .as_any()
                        .downcast_ref::<arrow::array::StructArray>()
                        .and_then(|struct_arr| {
                            if struct_arr.is_null(row_idx) {
                                None
                            } else {
                                struct_arr
                                    .column_by_name("id")
                                    .and_then(|id_col| {
                                        id_col.as_any().downcast_ref::<arrow::array::UInt16Array>()
                                    })
                                    .and_then(|id_arr| {
                                        if id_arr.is_null(row_idx) {
                                            None
                                        } else {
                                            Some(id_arr.value(row_idx))
                                        }
                                    })
                            }
                        })
                })
                .collect()
        } else {
            vec![None; num_rows]
        };

        // Now extract and attach resource attributes
        if let Some(res_attrs_batch) = resource_attrs_batch {
            Self::extract_and_attach_resource_attributes(&mut log_records, res_attrs_batch, &resource_ids)?;
        }

        Ok(log_records)
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

        // Buffer for accumulating logs before export
        let mut log_buffer: Vec<LogRecordData> = Vec::with_capacity(self.config.max_buffer_size);

        loop {
            match msg_chan.recv().await? {
                Message::Control(NodeControlMsg::Shutdown { deadline, .. }) => {
                    // Flush remaining logs before shutdown
                    if !log_buffer.is_empty() {
                        effect_handler
                            .info(&format!("Flushing {} logs before shutdown", log_buffer.len()))
                            .await;

                        if let Err(e) = self.geneva_client.encode_and_compress_otap_logs(&log_buffer)
                        {
                            effect_handler
                                .info(&format!("Failed to flush logs on shutdown: {}", e))
                                .await;
                        }
                    }

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
                            effect_handler
                                .info(&format!("📦 Processing OTAP Arrow batch with {} rows", otap_batch.get(otel_arrow_rust::proto::opentelemetry::arrow::v1::ArrowPayloadType::Logs).map(|b| b.num_rows()).unwrap_or(0)))
                                .await;

                            // Convert OTAP to LogRecordData
                            match Self::convert_otap_to_log_records(&otap_batch) {
                                Ok(mut records) => {
                                    effect_handler
                                        .info(&format!("✅ Converted {} log records from OTAP", records.len()))
                                        .await;

                                    log_buffer.append(&mut records);

                                    effect_handler
                                        .info(&format!("📊 Buffer now contains {} logs (max: {})", log_buffer.len(), self.config.max_buffer_size))
                                        .await;

                                    // Flush if buffer is full
                                    if log_buffer.len() >= self.config.max_buffer_size {
                                        effect_handler
                                            .info(&format!("🚀 Flushing {} logs to Geneva", log_buffer.len()))
                                            .await;

                                        match self
                                            .geneva_client
                                            .encode_and_compress_otap_logs(&log_buffer)
                                        {
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
                                        log_buffer.clear();
                                        effect_handler
                                            .info("🧹 Buffer cleared after flush")
                                            .await;
                                    }
                                }
                                Err(e) => {
                                    self.pdata_metrics.logs_failed.inc();
                                    effect_handler
                                        .info(&format!("❌ Failed to convert OTAP to logs: {}", e))
                                        .await;
                                }
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
