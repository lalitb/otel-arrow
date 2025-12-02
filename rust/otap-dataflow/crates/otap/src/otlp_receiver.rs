// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use crate::OTAP_RECEIVER_FACTORIES;
use crate::otap_grpc::otlp::server::{
    LogsServiceServer, MetricsServiceServer, RouteResponse, SharedState, TraceServiceServer,
};
use crate::otap_grpc::server_settings::GrpcServerSettings;
use crate::pdata::OtapPdata;
#[cfg(feature = "experimental-tls")]
use crate::tls_utils::{build_reloadable_server_config, create_tls_stream};
#[cfg(feature = "experimental-tls")]
use otap_df_config::tls::TlsServerConfig;

use crate::compression::CompressionMethod;
use async_trait::async_trait;
use linkme::distributed_slice;
use otap_df_config::SignalType;
use otap_df_config::node::NodeUserConfig;
use otap_df_engine::ReceiverFactory;
use otap_df_engine::config::ReceiverConfig;
use otap_df_engine::context::PipelineContext;
use otap_df_engine::control::{AckMsg, NackMsg, NodeControlMsg};
use otap_df_engine::error::{Error, ReceiverErrorKind, format_error_sources};
use otap_df_engine::node::NodeId;
use otap_df_engine::receiver::ReceiverWrapper;
use otap_df_engine::shared::receiver as shared;
use otap_df_engine::terminal_state::TerminalState;
use otap_df_telemetry::instrument::Counter;
use otap_df_telemetry::metrics::MetricSet;
use otap_df_telemetry_macros::metric_set;
use serde::Deserialize;
use serde_json::Value;
use std::ops::Add;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tonic::transport::Server;

/// URN for the OTLP Receiver
#[doc(hidden)]
pub const OTLP_RECEIVER_URN: &str = "urn:otel:otlp:receiver";

/// Configuration for OTLP Receiver
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// gRPC server settings
    #[serde(flatten)]
    pub settings: GrpcServerSettings,

    /// Compression methods accepted
    /// TODO: this should be (CompressionMethod, CompressionMethod), with separate settings
    /// (as tonic supports) for request and response compression.
    pub compression_method: Option<CompressionMethod>,

    /// TLS configuration
    #[cfg(feature = "experimental-tls")]
    pub tls: Option<TlsServerConfig>,
}

/// Receiver implementation that receives OTLP grpc service requests and decodes the data into OTAP.
pub struct OTLPReceiver {
    config: Config,
    metrics: MetricSet<OtlpReceiverMetrics>,
}

/// State shared between gRPC server task and the effect handler.
struct SharedStates {
    logs: Option<SharedState>,
    metrics: Option<SharedState>,
    traces: Option<SharedState>,
}

/// Declares the OTLP receiver as a shared receiver factory
///
#[allow(unsafe_code)]
#[distributed_slice(OTAP_RECEIVER_FACTORIES)]
pub static OTLP_RECEIVER: ReceiverFactory<OtapPdata> = ReceiverFactory {
    name: OTLP_RECEIVER_URN,
    create: |pipeline: PipelineContext,
             node: NodeId,
             node_config: Arc<NodeUserConfig>,
             receiver_config: &ReceiverConfig| {
        Ok(ReceiverWrapper::shared(
            OTLPReceiver::from_config(pipeline, &node_config.config)?,
            node,
            node_config,
            receiver_config,
        ))
    },
};

impl OTLPReceiver {
    /// Creates a new OTLPReceiver from a configuration object
    pub fn from_config(
        pipeline_ctx: PipelineContext,
        config: &Value,
    ) -> Result<Self, otap_df_config::error::Error> {
        let mut config: Config = serde_json::from_value(config.clone()).map_err(|e| {
            otap_df_config::error::Error::InvalidUserConfig {
                error: e.to_string(),
            }
        })?;

        // Map legacy compression_method to settings if needed
        if let Some(method) = config.compression_method {
            if config.settings.request_compression.is_none() {
                config.settings.request_compression = Some(vec![method]);
            }
        }

        // Register OTLP receiver metrics for this node.
        let metrics = pipeline_ctx.register_metrics::<OtlpReceiverMetrics>();

        Ok(Self { config, metrics })
    }

    fn route_ack_response(&self, states: &SharedStates, ack: AckMsg<OtapPdata>) -> RouteResponse {
        let calldata = ack.calldata;
        let resp = Ok(());
        let state = match ack.accepted.signal_type() {
            SignalType::Logs => states.logs.as_ref(),
            SignalType::Metrics => states.metrics.as_ref(),
            SignalType::Traces => states.traces.as_ref(),
        };

        state
            .map(|s| s.route_response(calldata, resp))
            .unwrap_or(RouteResponse::None)
    }

    fn route_nack_response(
        &self,
        states: &SharedStates,
        mut nack: NackMsg<OtapPdata>,
    ) -> RouteResponse {
        let calldata = std::mem::take(&mut nack.calldata);
        let signal_type = nack.refused.signal_type();
        let resp = Err(nack);
        let state = match signal_type {
            SignalType::Logs => states.logs.as_ref(),
            SignalType::Metrics => states.metrics.as_ref(),
            SignalType::Traces => states.traces.as_ref(),
        };

        state
            .map(|s| s.route_response(calldata, resp))
            .unwrap_or(RouteResponse::None)
    }

    fn handle_ack_response(&mut self, resp: RouteResponse) {
        match resp {
            RouteResponse::Sent => self.metrics.acks_sent.inc(),
            RouteResponse::Expired => self.metrics.acks_nacks_invalid_or_expired.inc(),
            RouteResponse::Invalid => self.metrics.acks_nacks_invalid_or_expired.inc(),
            RouteResponse::None => {}
        }
    }

    fn handle_nack_response(&mut self, resp: RouteResponse) {
        match resp {
            RouteResponse::Sent => self.metrics.nacks_sent.inc(),
            RouteResponse::Expired => self.metrics.acks_nacks_invalid_or_expired.inc(),
            RouteResponse::Invalid => self.metrics.acks_nacks_invalid_or_expired.inc(),
            RouteResponse::None => {}
        }
    }
}

/// OTLP receiver metrics.
//
// TODO: The following were unused, would have to be implemented in
// a different location:
//
// /// Number of bytes received.
// #[metric(unit = "By")]
// pub bytes_received: Counter<u64>,
// /// Number of messages received.
// #[metric(unit = "{msg}")]
// pub messages_received: Counter<u64>,
#[metric_set(name = "otlp.receiver.metrics")]
#[derive(Debug, Default, Clone)]
pub struct OtlpReceiverMetrics {
    /// Number of acks sent.
    #[metric(unit = "{acks}")]
    pub acks_sent: Counter<u64>,

    /// Number of nacks sent.
    #[metric(unit = "{nacks}")]
    pub nacks_sent: Counter<u64>,

    /// Number of invalid/expired acks/nacks.
    #[metric(unit = "{ack_or_nack}")]
    pub acks_nacks_invalid_or_expired: Counter<u64>,
}

#[async_trait]
impl shared::Receiver<OtapPdata> for OTLPReceiver {
    async fn start(
        mut self: Box<Self>,
        mut ctrl_msg_recv: shared::ControlChannel<OtapPdata>,
        effect_handler: shared::EffectHandler<OtapPdata>,
    ) -> Result<TerminalState, Error> {
        // Make the receiver mutable so we can update metrics on telemetry collection.
        let listener = effect_handler.tcp_listener(self.config.settings.listening_addr)?;
        let listener_stream = self.config.settings.build_tcp_incoming(listener);

        let settings = self.config.settings.build_settings();

        let logs_server = LogsServiceServer::new(effect_handler.clone(), &settings);
        let metrics_server = MetricsServiceServer::new(effect_handler.clone(), &settings);
        let traces_server = TraceServiceServer::new(effect_handler.clone(), &settings);

        let states = SharedStates {
            logs: logs_server.common.state(),
            metrics: metrics_server.common.state(),
            traces: traces_server.common.state(),
        };

        let mut server_builder = Server::builder();

        // Apply timeout if configured
        if let Some(timeout) = self.config.settings.timeout {
            server_builder = server_builder.timeout(timeout);
        }

        #[cfg(feature = "experimental-tls")]
        let maybe_tls_acceptor = if let Some(tls_config) = &self.config.tls {
            let server_config = build_reloadable_server_config(tls_config)
                .await
                .map_err(|e| Error::ReceiverError {
                    receiver: effect_handler.receiver_id(),
                    kind: ReceiverErrorKind::Configuration,
                    error: format!("Failed to configure TLS: {}", e),
                    source_detail: format_error_sources(&e),
                })?;
            Some(tokio_rustls::TlsAcceptor::from(server_config))
        } else {
            None
        };

        let server = server_builder
            .add_service(logs_server)
            .add_service(metrics_server)
            .add_service(traces_server);

        // Start periodic telemetry collection
        let telemetry_cancel_handle = effect_handler
            .start_periodic_telemetry(Duration::from_secs(1))
            .await?;

        tokio::select! {
            biased;

            // Process internal events
            ctrl_msg_result = async {
                loop {
                    match ctrl_msg_recv.recv().await {
                        Ok(NodeControlMsg::Shutdown { deadline, .. }) => {
                            let snapshot = self.metrics.snapshot();
                            _ = telemetry_cancel_handle.cancel().await;
                            return Ok(TerminalState::new(deadline, [snapshot]));
                        },
                        Ok(NodeControlMsg::CollectTelemetry { mut metrics_reporter }) => {
                            // Report current receiver metrics.
                            _ = metrics_reporter.report(&mut self.metrics);
                        },
                        Ok(NodeControlMsg::Ack(ack)) => {
                            self.handle_ack_response(self.route_ack_response(&states, ack));
                        },
                        Ok(NodeControlMsg::Nack(nack)) => {
                            self.handle_nack_response(self.route_nack_response(&states, nack));
                        },
                        Err(e) => {
                            return Err(Error::ChannelRecvError(e));
                        }
                        _ => {
                            // unknown control message do nothing
                        }
                    }
                }
            } => {
                return ctrl_msg_result;
            },

            // Run server
            result = async {
                #[cfg(feature = "experimental-tls")]
                match maybe_tls_acceptor {
                    Some(tls_acceptor) => {
                        let tls_stream = create_tls_stream(listener_stream, tls_acceptor);
                        server.serve_with_incoming(tls_stream).await
                    }
                    None => {
                        server.serve_with_incoming(listener_stream).await
                    }
                }
                #[cfg(not(feature = "experimental-tls"))]
                {
                    server.serve_with_incoming(listener_stream).await
                }
            } => {
                if let Err(error) = result {
                    let source_detail = format_error_sources(&error);
                    return Err(Error::ReceiverError {
                        receiver: effect_handler.receiver_id(),
                        kind: ReceiverErrorKind::Transport,
                        error: error.to_string(),
                        source_detail,
                    });
                }
            }
        }

        Ok(TerminalState::new(
            Instant::now().add(Duration::from_secs(1)),
            [self.metrics],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use otap_df_config::node::NodeUserConfig;
    use otap_df_engine::context::ControllerContext;
    use otap_df_engine::control::NackMsg;
    use otap_df_engine::control::{AckMsg, NodeControlMsg};
    use otap_df_engine::receiver::ReceiverWrapper;
    use otap_df_engine::testing::{
        receiver::{NotSendValidateContext, TestContext, TestRuntime},
        test_node,
    };
    use otap_df_pdata::OtlpProtoBytes;
    use otap_df_pdata::proto::opentelemetry::collector::logs::v1::logs_service_client::LogsServiceClient;
    use otap_df_pdata::proto::opentelemetry::collector::logs::v1::{
        ExportLogsServiceRequest, ExportLogsServiceResponse,
    };
    use otap_df_pdata::proto::opentelemetry::collector::metrics::v1::metrics_service_client::MetricsServiceClient;
    use otap_df_pdata::proto::opentelemetry::collector::metrics::v1::{
        ExportMetricsServiceRequest, ExportMetricsServiceResponse,
    };
    use otap_df_pdata::proto::opentelemetry::collector::trace::v1::trace_service_client::TraceServiceClient;
    use otap_df_pdata::proto::opentelemetry::collector::trace::v1::{
        ExportTraceServiceRequest, ExportTraceServiceResponse,
    };
    use otap_df_pdata::proto::opentelemetry::common::v1::{InstrumentationScope, KeyValue};
    use otap_df_pdata::proto::opentelemetry::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use otap_df_pdata::proto::opentelemetry::metrics::v1::{ResourceMetrics, ScopeMetrics};
    use otap_df_pdata::proto::opentelemetry::resource::v1::Resource;
    use otap_df_pdata::proto::opentelemetry::trace::v1::{ResourceSpans, ScopeSpans};
    use otap_df_telemetry::registry::MetricsRegistryHandle;
    use prost::Message;
    use std::pin::Pin;
    use std::time::{Duration, Instant};
    use tokio::time::timeout;

    fn create_logs_service_request() -> ExportLogsServiceRequest {
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "a".to_string(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                scope_logs: vec![ScopeLogs {
                    scope: Some(InstrumentationScope {
                        attributes: vec![KeyValue {
                            key: "b".to_string(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    log_records: vec![
                        LogRecord {
                            time_unix_nano: 1,
                            attributes: vec![KeyValue {
                                key: "c".to_string(),
                                ..Default::default()
                            }],
                            ..Default::default()
                        },
                        LogRecord {
                            time_unix_nano: 2,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    fn create_metrics_service_request() -> ExportMetricsServiceRequest {
        ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: Some(Resource {
                    ..Default::default()
                }),
                scope_metrics: vec![ScopeMetrics {
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    fn create_traces_service_request() -> ExportTraceServiceRequest {
        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: None,
                scope_spans: vec![
                    ScopeSpans {
                        ..Default::default()
                    },
                    ScopeSpans {
                        ..Default::default()
                    },
                ],
                schema_url: "opentelemetry.io/schema/traces".to_string(),
            }],
        }
    }

    #[test]
    fn test_config_parsing() {
        use serde_json::json;

        let metrics_registry_handle = MetricsRegistryHandle::new();
        let controller_ctx = ControllerContext::new(metrics_registry_handle);
        let pipeline_ctx =
            controller_ctx.pipeline_context_with("grp".into(), "pipeline".into(), 0, 0);

        let config_with_max_concurrent_requests = json!({
            "listening_addr": "127.0.0.1:4317",
            "max_concurrent_requests": 5000
        });
        let receiver =
            OTLPReceiver::from_config(pipeline_ctx.clone(), &config_with_max_concurrent_requests)
                .unwrap();
        assert_eq!(receiver.config.settings.max_concurrent_requests, 5000);

        let config_default = json!({
            "listening_addr": "127.0.0.1:4317"
        });
        let receiver = OTLPReceiver::from_config(pipeline_ctx.clone(), &config_default).unwrap();
        assert_eq!(receiver.config.settings.max_concurrent_requests, 0); // Default is 0 in GrpcServerSettings

        let config_full = json!({
            "listening_addr": "127.0.0.1:4317",
            "compression_method": "gzip",
            "max_concurrent_requests": 2500
        });
        let receiver = OTLPReceiver::from_config(pipeline_ctx.clone(), &config_full).unwrap();
        assert_eq!(receiver.config.settings.max_concurrent_requests, 2500);
        // Check if compression_method was mapped to request_compression
        assert!(receiver.config.settings.request_compression.is_some());
        assert_eq!(
            receiver
                .config
                .settings
                .request_compression
                .as_ref()
                .unwrap()[0],
            CompressionMethod::Gzip
        );

        let config_with_timeout = json!({
            "listening_addr": "127.0.0.1:4317",
            "timeout": "30s"
        });
        let receiver =
            OTLPReceiver::from_config(pipeline_ctx.clone(), &config_with_timeout).unwrap();
        assert_eq!(
            receiver.config.settings.timeout,
            Some(Duration::from_secs(30))
        );

        let config_with_timeout_ms = json!({
            "listening_addr": "127.0.0.1:4317",
            "timeout": "500ms"
        });
        let receiver =
            OTLPReceiver::from_config(pipeline_ctx.clone(), &config_with_timeout_ms).unwrap();
        assert_eq!(
            receiver.config.settings.timeout,
            Some(Duration::from_millis(500))
        );

        #[cfg(feature = "experimental-tls")]
        {
            let config_tls = json!({
                "listening_addr": "127.0.0.1:4317",
                "tls": {
                    "cert_file": "/path/to/cert",
                    "key_file": "/path/to/key",
                    "client_ca_file": "/path/to/ca"
                }
            });
            let receiver = OTLPReceiver::from_config(pipeline_ctx, &config_tls).unwrap();
            let tls = receiver.config.tls.as_ref().unwrap();
            assert_eq!(tls.config.cert_file, Some(PathBuf::from("/path/to/cert")));
            assert_eq!(tls.config.key_file, Some(PathBuf::from("/path/to/key")));
            assert_eq!(tls.client_ca_file, Some(PathBuf::from("/path/to/ca")));
        }
    }

    #[test]
    fn test_multi_compression_config() {
        use crate::compression::CompressionMethod;
        use serde_json::json;

        let metrics_registry_handle = MetricsRegistryHandle::new();
        let controller_ctx = ControllerContext::new(metrics_registry_handle);
        let pipeline_ctx =
            controller_ctx.pipeline_context_with("grp".into(), "pipeline".into(), 0, 0);

        // Test new multi-compression config
        let config = json!({
            "listening_addr": "127.0.0.1:4317",
            "request_compression": ["zstd", "gzip"],
            "response_compression": ["zstd"]
        });
        let receiver = OTLPReceiver::from_config(pipeline_ctx.clone(), &config).unwrap();
        assert_eq!(
            receiver.config.settings.request_compression_methods(),
            vec![CompressionMethod::Zstd, CompressionMethod::Gzip]
        );
        assert_eq!(
            receiver.config.settings.response_compression_methods(),
            vec![CompressionMethod::Zstd]
        );

        // Test legacy compression_method maps to request_compression
        let config_legacy = json!({
            "listening_addr": "127.0.0.1:4318",
            "compression_method": "gzip"
        });
        let receiver = OTLPReceiver::from_config(pipeline_ctx.clone(), &config_legacy).unwrap();
        assert_eq!(
            receiver.config.settings.request_compression_methods(),
            vec![CompressionMethod::Gzip]
        );

        // Test explicit request_compression takes precedence over legacy
        let config_both = json!({
            "listening_addr": "127.0.0.1:4319",
            "compression_method": "gzip",
            "request_compression": ["zstd"]
        });
        let receiver = OTLPReceiver::from_config(pipeline_ctx.clone(), &config_both).unwrap();
        assert_eq!(
            receiver.config.settings.request_compression_methods(),
            vec![CompressionMethod::Zstd]
        );

        // Test defaults: request accepts all, response is empty
        let config_defaults = json!({
            "listening_addr": "127.0.0.1:4320"
        });
        let receiver = OTLPReceiver::from_config(pipeline_ctx, &config_defaults).unwrap();
        assert_eq!(
            receiver.config.settings.request_compression_methods(),
            vec![
                CompressionMethod::Zstd,
                CompressionMethod::Gzip,
                CompressionMethod::Deflate
            ]
        );
        assert!(
            receiver
                .config
                .settings
                .response_compression_methods()
                .is_empty()
        );
    }

    fn scenario(
        grpc_endpoint: String,
    ) -> impl FnOnce(TestContext<OtapPdata>) -> Pin<Box<dyn Future<Output = ()>>> {
        move |ctx| {
            Box::pin(async move {
                let mut logs_client = LogsServiceClient::connect(grpc_endpoint.clone())
                    .await
                    .expect("Failed to connect to server from Logs Service Client");

                let logs_response = logs_client
                    .export(create_logs_service_request())
                    .await
                    .expect("Can send log request")
                    .into_inner();
                assert_eq!(
                    logs_response,
                    ExportLogsServiceResponse {
                        partial_success: None
                    }
                );

                let mut metrics_client = MetricsServiceClient::connect(grpc_endpoint.clone())
                    .await
                    .expect("Failed to connect to server from Metrics Service Client");
                let metrics_response = metrics_client
                    .export(create_metrics_service_request())
                    .await
                    .expect("can send metrics request")
                    .into_inner();
                assert_eq!(
                    metrics_response,
                    ExportMetricsServiceResponse {
                        partial_success: None
                    }
                );

                let mut traces_client = TraceServiceClient::connect(grpc_endpoint.clone())
                    .await
                    .expect("Failed to connect to server from Traces Service Client");
                let traces_response = traces_client
                    .export(create_traces_service_request())
                    .await
                    .expect("can send traces request")
                    .into_inner();
                assert_eq!(
                    traces_response,
                    ExportTraceServiceResponse {
                        partial_success: None
                    }
                );

                ctx.send_shutdown(Instant::now(), "Test")
                    .await
                    .expect("Failed to send Shutdown");

                let fail_client = LogsServiceClient::connect(grpc_endpoint.clone()).await;
                assert!(fail_client.is_err(), "Server did not shutdown");
            })
        }
    }

    fn validation_procedure()
    -> impl FnOnce(NotSendValidateContext<OtapPdata>) -> Pin<Box<dyn Future<Output = ()>>> {
        |mut ctx| {
            Box::pin(async move {
                // Receive logs pdata
                let logs_pdata = timeout(Duration::from_secs(3), ctx.recv())
                    .await
                    .expect("Timed out waiting for logs message")
                    .expect("No logs message received");

                // Validate logs payload
                let logs_proto: OtlpProtoBytes = logs_pdata
                    .clone()
                    .payload()
                    .try_into()
                    .expect("can convert to OtlpProtoBytes");
                assert!(matches!(logs_proto, OtlpProtoBytes::ExportLogsRequest(_)));

                let expected = create_logs_service_request();
                let mut expected_bytes = Vec::new();
                expected.encode(&mut expected_bytes).unwrap();
                assert_eq!(&expected_bytes, logs_proto.as_bytes());

                // Send Ack back to unblock the gRPC handler
                if let Some((_node_id, ack)) =
                    crate::pdata::Context::next_ack(AckMsg::new(logs_pdata))
                {
                    ctx.send_control_msg(NodeControlMsg::Ack(ack))
                        .await
                        .expect("Failed to send Ack for logs");
                }

                // Receive metrics pdata
                let metrics_pdata = timeout(Duration::from_secs(3), ctx.recv())
                    .await
                    .expect("Timed out waiting for metrics message")
                    .expect("No metrics message received");

                // Validate metrics payload
                let metrics_proto: OtlpProtoBytes = metrics_pdata
                    .clone()
                    .payload()
                    .try_into()
                    .expect("can convert to OtlpProtoBytes");
                assert!(matches!(
                    metrics_proto,
                    OtlpProtoBytes::ExportMetricsRequest(_)
                ));

                let expected = create_metrics_service_request();
                let mut expected_bytes = Vec::new();
                expected.encode(&mut expected_bytes).unwrap();
                assert_eq!(&expected_bytes, metrics_proto.as_bytes());

                // Send Ack back to unblock the gRPC handler
                if let Some((_node_id, ack)) =
                    crate::pdata::Context::next_ack(AckMsg::new(metrics_pdata))
                {
                    ctx.send_control_msg(NodeControlMsg::Ack(ack))
                        .await
                        .expect("Failed to send Ack for metrics");
                }

                // Receive trace pdata
                let trace_pdata = timeout(Duration::from_secs(3), ctx.recv())
                    .await
                    .expect("Timed out waiting for trace message")
                    .expect("No trace message received");

                // Validate trace payload
                let trace_proto: OtlpProtoBytes = trace_pdata
                    .clone()
                    .payload()
                    .try_into()
                    .expect("can convert to OtlpProtoBytes");
                assert!(matches!(
                    trace_proto,
                    OtlpProtoBytes::ExportTracesRequest(_)
                ));

                let expected = create_traces_service_request();
                let mut expected_bytes = Vec::new();
                expected.encode(&mut expected_bytes).unwrap();
                assert_eq!(&expected_bytes, trace_proto.as_bytes());

                // Send Ack back to unblock the gRPC handler
                if let Some((_node_id, ack)) =
                    crate::pdata::Context::next_ack(AckMsg::new(trace_pdata))
                {
                    ctx.send_control_msg(NodeControlMsg::Ack(ack))
                        .await
                        .expect("Failed to send Ack for traces");
                }
            })
        }
    }

    #[test]
    fn test_otlp_receiver_ack() {
        let test_runtime = TestRuntime::new();

        let grpc_addr = "127.0.0.1";
        let grpc_port = portpicker::pick_unused_port().expect("No free ports");
        let grpc_endpoint = format!("http://{grpc_addr}:{grpc_port}");
        let addr: SocketAddr = format!("{grpc_addr}:{grpc_port}").parse().unwrap();

        let node_config = Arc::new(NodeUserConfig::new_receiver_config(OTLP_RECEIVER_URN));

        // Create a proper pipeline context for the test
        let metrics_registry_handle = MetricsRegistryHandle::new();
        let controller_ctx = ControllerContext::new(metrics_registry_handle);
        let pipeline_ctx =
            controller_ctx.pipeline_context_with("grp".into(), "pipeline".into(), 0, 0);

        let receiver = ReceiverWrapper::shared(
            OTLPReceiver {
                config: Config {
                    settings: GrpcServerSettings {
                        wait_for_result: true,
                        listening_addr: addr,
                        max_concurrent_requests: 1000,
                        timeout: None,
                        ..Default::default()
                    },
                    compression_method: None,
                    #[cfg(feature = "experimental-tls")]
                    tls: None,
                },
                metrics: pipeline_ctx.register_metrics::<OtlpReceiverMetrics>(),
            },
            test_node(test_runtime.config().name.clone()),
            node_config,
            test_runtime.config(),
        );

        test_runtime
            .set_receiver(receiver)
            .run_test(scenario(grpc_endpoint))
            .run_validation_concurrent(validation_procedure());
    }

    #[test]
    fn test_otlp_receiver_nack() {
        let test_runtime = TestRuntime::new();

        let grpc_addr = "127.0.0.1";
        let grpc_port = portpicker::pick_unused_port().expect("No free ports");
        let grpc_endpoint = format!("http://{grpc_addr}:{grpc_port}");
        let addr: SocketAddr = format!("{grpc_addr}:{grpc_port}").parse().unwrap();

        let node_config = Arc::new(NodeUserConfig::new_receiver_config(OTLP_RECEIVER_URN));

        let metrics_registry_handle = MetricsRegistryHandle::new();
        let controller_ctx = ControllerContext::new(metrics_registry_handle);
        let pipeline_ctx =
            controller_ctx.pipeline_context_with("grp".into(), "pipeline".into(), 0, 0);

        let receiver = ReceiverWrapper::shared(
            OTLPReceiver {
                config: Config {
                    settings: GrpcServerSettings {
                        wait_for_result: true,
                        listening_addr: addr,
                        max_concurrent_requests: 1000,
                        timeout: None,
                        ..Default::default()
                    },
                    compression_method: None,
                    #[cfg(feature = "experimental-tls")]
                    tls: None,
                },
                metrics: pipeline_ctx.register_metrics::<OtlpReceiverMetrics>(),
            },
            test_node(test_runtime.config().name.clone()),
            node_config,
            test_runtime.config(),
        );

        let nack_scenario = move |ctx: TestContext<OtapPdata>| {
            Box::pin(async move {
                let mut logs_client = LogsServiceClient::connect(grpc_endpoint.clone())
                    .await
                    .expect("Failed to connect to server");

                let result = logs_client.export(create_logs_service_request()).await;

                assert!(result.is_err(), "Expected error response");
                let status = result.unwrap_err();

                // Verify we get UNAVAILABLE status code
                assert_eq!(status.code(), tonic::Code::Unavailable);
                assert!(status.message().contains("Test nack reason"));
                assert!(status.message().contains("Pipeline processing failed"));

                ctx.send_shutdown(Instant::now(), "Test complete")
                    .await
                    .expect("Failed to send shutdown");
            }) as Pin<Box<dyn Future<Output = ()>>>
        };

        let nack_validation = |mut ctx: NotSendValidateContext<OtapPdata>| {
            Box::pin(async move {
                // Receive the logs pdata, create Nack message and send it back
                let logs_pdata = timeout(Duration::from_secs(3), ctx.recv())
                    .await
                    .expect("Timed out waiting for logs message")
                    .expect("No logs message received");

                let nack = NackMsg::new("Test nack reason", logs_pdata);
                if let Some((_node_id, nack)) = crate::pdata::Context::next_nack(nack) {
                    ctx.send_control_msg(NodeControlMsg::Nack(nack))
                        .await
                        .expect("Failed to send Nack");
                }
            }) as Pin<Box<dyn Future<Output = ()>>>
        };

        test_runtime
            .set_receiver(receiver)
            .run_test(nack_scenario)
            .run_validation_concurrent(nack_validation);
    }

    #[test]
    #[cfg(feature = "experimental-tls")]
    fn test_otlp_receiver_tls() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let test_runtime = TestRuntime::new();

        let grpc_addr = "127.0.0.1";
        let grpc_port = portpicker::pick_unused_port().expect("No free ports");
        // Note: https scheme
        let grpc_endpoint = format!("https://{grpc_addr}:{grpc_port}");
        let addr: SocketAddr = format!("{grpc_addr}:{grpc_port}").parse().unwrap();

        let node_config = Arc::new(NodeUserConfig::new_receiver_config(OTLP_RECEIVER_URN));

        let metrics_registry_handle = MetricsRegistryHandle::new();
        let controller_ctx = ControllerContext::new(metrics_registry_handle);
        let pipeline_ctx =
            controller_ctx.pipeline_context_with("grp".into(), "pipeline".into(), 0, 0);

        // Generate certs in a temp dir
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        crate::testing::generate_test_certs(temp_dir.path());
        let cert_path = temp_dir.path().join("server.crt");
        let key_path = temp_dir.path().join("server.key");
        let ca_path = temp_dir.path().join("ca.crt");

        // Read certs into memory to test in-memory config (and avoid runtime disk dependency)
        let cert_pem = std::fs::read_to_string(&cert_path).expect("failed to read server cert");
        let key_pem = std::fs::read_to_string(&key_path).expect("failed to read server key");
        let ca_pem = std::fs::read_to_string(&ca_path).expect("failed to read ca cert");

        let receiver = ReceiverWrapper::shared(
            OTLPReceiver {
                config: Config {
                    settings: GrpcServerSettings {
                        wait_for_result: true,
                        listening_addr: addr,
                        max_concurrent_requests: 1000,
                        timeout: None,
                        ..Default::default()
                    },
                    compression_method: None,
                    tls: Some(TlsServerConfig {
                        config: otap_df_config::tls::TlsConfig {
                            cert_pem: Some(cert_pem),
                            key_pem: Some(key_pem),
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                },
                metrics: pipeline_ctx.register_metrics::<OtlpReceiverMetrics>(),
            },
            test_node(test_runtime.config().name.clone()),
            node_config,
            test_runtime.config(),
        );

        let tls_scenario = move |ctx: TestContext<OtapPdata>| {
            Box::pin(async move {
                // Use the in-memory CA pem
                let ca_cert = tonic::transport::Certificate::from_pem(ca_pem);

                let tls_config = tonic::transport::ClientTlsConfig::new()
                    .ca_certificate(ca_cert)
                    .domain_name("localhost");

                let channel = timeout(
                    Duration::from_secs(5),
                    tonic::transport::Channel::from_shared(grpc_endpoint.clone())
                        .expect("Invalid URI")
                        .tls_config(tls_config)
                        .expect("Failed to configure TLS")
                        .connect(),
                )
                .await
                .expect("Connection timed out")
                .expect("Failed to connect");

                let mut logs_client = LogsServiceClient::new(channel.clone());

                let logs_response = logs_client
                    .export(create_logs_service_request())
                    .await
                    .expect("Can send log request")
                    .into_inner();

                assert_eq!(
                    logs_response,
                    ExportLogsServiceResponse {
                        partial_success: None
                    }
                );

                let mut metrics_client = MetricsServiceClient::new(channel.clone());
                let metrics_response = metrics_client
                    .export(create_metrics_service_request())
                    .await
                    .expect("can send metrics request")
                    .into_inner();
                assert_eq!(
                    metrics_response,
                    ExportMetricsServiceResponse {
                        partial_success: None
                    }
                );

                let mut traces_client = TraceServiceClient::new(channel);
                let traces_response = traces_client
                    .export(create_traces_service_request())
                    .await
                    .expect("can send traces request")
                    .into_inner();
                assert_eq!(
                    traces_response,
                    ExportTraceServiceResponse {
                        partial_success: None
                    }
                );

                ctx.send_shutdown(Instant::now(), "Test")
                    .await
                    .expect("Failed to send Shutdown");
            }) as Pin<Box<dyn Future<Output = ()>>>
        };

        test_runtime
            .set_receiver(receiver)
            .run_test(tls_scenario)
            .run_validation_concurrent(validation_procedure());
    }

    #[cfg(feature = "experimental-tls")]
    fn generate_client_certs(dir: &std::path::Path) {
        use std::process::Command;
        // Generate Client Key and CSR
        let status = Command::new("openssl")
            .args([
                "req",
                "-newkey",
                "rsa:2048",
                "-keyout",
                "client.key",
                "-out",
                "client.csr",
                "-nodes",
                "-subj",
                "/CN=client",
            ])
            .current_dir(dir)
            .output()
            .expect("Failed to generate client CSR");
        if !status.status.success() {
            panic!(
                "Client CSR gen failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );
        }

        // Sign Client CSR with CA
        let status = Command::new("openssl")
            .args([
                "x509",
                "-req",
                "-in",
                "client.csr",
                "-CA",
                "ca.crt",
                "-CAkey",
                "ca.key",
                "-CAcreateserial",
                "-out",
                "client.crt",
                "-days",
                "365",
            ])
            .current_dir(dir)
            .output()
            .expect("Failed to sign client cert");
        if !status.status.success() {
            panic!(
                "Client Sign failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );
        }
    }

    #[test]
    #[cfg(feature = "experimental-tls")]
    fn test_otlp_receiver_tls_file_based() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let test_runtime = TestRuntime::new();

        let grpc_addr = "127.0.0.1";
        let grpc_port = portpicker::pick_unused_port().expect("No free ports");
        let grpc_endpoint = format!("https://{grpc_addr}:{grpc_port}");
        let addr: SocketAddr = format!("{grpc_addr}:{grpc_port}").parse().unwrap();

        let node_config = Arc::new(NodeUserConfig::new_receiver_config(OTLP_RECEIVER_URN));

        let metrics_registry_handle = MetricsRegistryHandle::new();
        let controller_ctx = ControllerContext::new(metrics_registry_handle);
        let pipeline_ctx =
            controller_ctx.pipeline_context_with("grp".into(), "pipeline".into(), 0, 0);

        // Generate certs in a temp dir
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        crate::testing::generate_test_certs(temp_dir.path());
        let cert_path = temp_dir.path().join("server.crt");
        let key_path = temp_dir.path().join("server.key");
        let ca_path = temp_dir.path().join("ca.crt");

        // Read CA pem for client config
        let ca_pem = std::fs::read_to_string(&ca_path).expect("failed to read ca cert");

        let receiver = ReceiverWrapper::shared(
            OTLPReceiver {
                config: Config {
                    settings: GrpcServerSettings {
                        wait_for_result: true,
                        listening_addr: addr,
                        max_concurrent_requests: 1000,
                        timeout: None,
                        ..Default::default()
                    },
                    compression_method: None,
                    tls: Some(TlsServerConfig {
                        config: otap_df_config::tls::TlsConfig {
                            cert_file: Some(cert_path),
                            key_file: Some(key_path),
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                },
                metrics: pipeline_ctx.register_metrics::<OtlpReceiverMetrics>(),
            },
            test_node(test_runtime.config().name.clone()),
            node_config,
            test_runtime.config(),
        );

        let tls_scenario = move |ctx: TestContext<OtapPdata>| {
            Box::pin(async move {
                let ca_cert = tonic::transport::Certificate::from_pem(ca_pem);
                let tls_config = tonic::transport::ClientTlsConfig::new()
                    .ca_certificate(ca_cert)
                    .domain_name("localhost");

                let channel = timeout(
                    Duration::from_secs(5),
                    tonic::transport::Channel::from_shared(grpc_endpoint.clone())
                        .expect("Invalid URI")
                        .tls_config(tls_config)
                        .expect("Failed to configure TLS")
                        .connect(),
                )
                .await
                .expect("Connection timed out")
                .expect("Failed to connect");

                let mut logs_client = LogsServiceClient::new(channel.clone());
                let logs_response = logs_client
                    .export(create_logs_service_request())
                    .await
                    .expect("Can send log request")
                    .into_inner();

                assert_eq!(
                    logs_response,
                    ExportLogsServiceResponse {
                        partial_success: None
                    }
                );

                let mut metrics_client = MetricsServiceClient::new(channel.clone());
                let metrics_response = metrics_client
                    .export(create_metrics_service_request())
                    .await
                    .expect("can send metrics request")
                    .into_inner();
                assert_eq!(
                    metrics_response,
                    ExportMetricsServiceResponse {
                        partial_success: None
                    }
                );

                let mut traces_client = TraceServiceClient::new(channel);
                let traces_response = traces_client
                    .export(create_traces_service_request())
                    .await
                    .expect("can send traces request")
                    .into_inner();
                assert_eq!(
                    traces_response,
                    ExportTraceServiceResponse {
                        partial_success: None
                    }
                );

                ctx.send_shutdown(Instant::now(), "Test")
                    .await
                    .expect("Failed to send Shutdown");
            }) as Pin<Box<dyn Future<Output = ()>>>
        };

        test_runtime
            .set_receiver(receiver)
            .run_test(tls_scenario)
            .run_validation_concurrent(validation_procedure());
    }

    #[test]
    #[cfg(feature = "experimental-tls")]
    fn test_otlp_receiver_mtls() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let test_runtime = TestRuntime::new();

        let grpc_addr = "127.0.0.1";
        let grpc_port = portpicker::pick_unused_port().expect("No free ports");
        let grpc_endpoint = format!("https://{grpc_addr}:{grpc_port}");
        let addr: SocketAddr = format!("{grpc_addr}:{grpc_port}").parse().unwrap();

        let node_config = Arc::new(NodeUserConfig::new_receiver_config(OTLP_RECEIVER_URN));

        let metrics_registry_handle = MetricsRegistryHandle::new();
        let controller_ctx = ControllerContext::new(metrics_registry_handle);
        let pipeline_ctx =
            controller_ctx.pipeline_context_with("grp".into(), "pipeline".into(), 0, 0);

        // Generate certs in a temp dir
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        crate::testing::generate_test_certs(temp_dir.path());
        generate_client_certs(temp_dir.path());

        let cert_path = temp_dir.path().join("server.crt");
        let key_path = temp_dir.path().join("server.key");
        let ca_path = temp_dir.path().join("ca.crt");
        let client_cert_path = temp_dir.path().join("client.crt");
        let client_key_path = temp_dir.path().join("client.key");

        // Read certs
        let ca_pem = std::fs::read_to_string(&ca_path).expect("failed to read ca cert");
        let client_cert_pem =
            std::fs::read_to_string(&client_cert_path).expect("failed to read client cert");
        let client_key_pem =
            std::fs::read_to_string(&client_key_path).expect("failed to read client key");

        let receiver = ReceiverWrapper::shared(
            OTLPReceiver {
                config: Config {
                    settings: GrpcServerSettings {
                        wait_for_result: true,
                        listening_addr: addr,
                        max_concurrent_requests: 1000,
                        timeout: None,
                        ..Default::default()
                    },
                    compression_method: None,
                    tls: Some(TlsServerConfig {
                        config: otap_df_config::tls::TlsConfig {
                            cert_file: Some(cert_path),
                            key_file: Some(key_path),
                            ..Default::default()
                        },
                        client_ca_file: Some(ca_path), // Enable mTLS
                        client_crl_file: None,
                        client_ca_pem: None,
                        include_system_ca_certs_pool: None,
                    }),
                },
                metrics: pipeline_ctx.register_metrics::<OtlpReceiverMetrics>(),
            },
            test_node(test_runtime.config().name.clone()),
            node_config,
            test_runtime.config(),
        );

        let mtls_scenario = move |ctx: TestContext<OtapPdata>| {
            Box::pin(async move {
                let ca_cert = tonic::transport::Certificate::from_pem(ca_pem);
                let client_identity =
                    tonic::transport::Identity::from_pem(client_cert_pem, client_key_pem);

                let tls_config = tonic::transport::ClientTlsConfig::new()
                    .ca_certificate(ca_cert)
                    .identity(client_identity)
                    .domain_name("localhost");

                let channel = timeout(
                    Duration::from_secs(5),
                    tonic::transport::Channel::from_shared(grpc_endpoint.clone())
                        .expect("Invalid URI")
                        .tls_config(tls_config)
                        .expect("Failed to configure TLS")
                        .connect(),
                )
                .await
                .expect("Connection timed out")
                .expect("Failed to connect");

                let mut logs_client = LogsServiceClient::new(channel.clone());
                let logs_response = logs_client
                    .export(create_logs_service_request())
                    .await
                    .expect("Can send log request")
                    .into_inner();

                assert_eq!(
                    logs_response,
                    ExportLogsServiceResponse {
                        partial_success: None
                    }
                );

                let mut metrics_client = MetricsServiceClient::new(channel.clone());
                let metrics_response = metrics_client
                    .export(create_metrics_service_request())
                    .await
                    .expect("can send metrics request")
                    .into_inner();
                assert_eq!(
                    metrics_response,
                    ExportMetricsServiceResponse {
                        partial_success: None
                    }
                );

                let mut traces_client = TraceServiceClient::new(channel);
                let traces_response = traces_client
                    .export(create_traces_service_request())
                    .await
                    .expect("can send traces request")
                    .into_inner();
                assert_eq!(
                    traces_response,
                    ExportTraceServiceResponse {
                        partial_success: None
                    }
                );

                ctx.send_shutdown(Instant::now(), "Test")
                    .await
                    .expect("Failed to send Shutdown");
            }) as Pin<Box<dyn Future<Output = ()>>>
        };

        test_runtime
            .set_receiver(receiver)
            .run_test(mtls_scenario)
            .run_validation_concurrent(validation_procedure());
    }
}
