// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use crate::OTAP_RECEIVER_FACTORIES;
use crate::otap_grpc::otlp::server::{RouteResponse, SharedState, SlotGuard};
use crate::pdata::{Context, OtapPdata};
use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
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
use otap_df_engine::{Interests, ProducerEffectHandlerExtension};
use otap_df_pdata::OtlpProtoBytes;
use otap_df_pdata::proto::opentelemetry::collector::logs::v1::ExportLogsServiceResponse;
use otap_df_pdata::proto::opentelemetry::collector::metrics::v1::ExportMetricsServiceResponse;
use otap_df_pdata::proto::opentelemetry::collector::trace::v1::ExportTraceServiceResponse;
use otap_df_telemetry::instrument::Counter;
use otap_df_telemetry::metrics::MetricSet;
use otap_df_telemetry_macros::metric_set;
use prost::Message;
use serde::Deserialize;
use serde_json::Value;
use std::net::SocketAddr;
use std::ops::Add;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// URN for the OTLP HTTP Receiver
pub const OTLP_HTTP_RECEIVER_URN: &str = "urn:otel:otlp-http:receiver";

/// Configuration for OTLP HTTP Receiver
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The endpoint details: protocol, name, port.
    listening_addr: SocketAddr,

    /// Maximum number of concurrent (in-flight) requests (default: 1000)
    #[serde(default = "default_max_concurrent_requests")]
    max_concurrent_requests: usize,

    /// Whether to wait for the result (default: true)
    #[serde(default = "default_wait_for_result")]
    wait_for_result: bool,

    /// Timeout for HTTP requests. If not specified, no timeout is applied.
    /// Format: humantime format (e.g., "30s", "5m", "1h", "500ms")
    #[serde(default, with = "humantime_serde")]
    pub timeout: Option<Duration>,
}

const fn default_max_concurrent_requests() -> usize {
    1000
}

const fn default_wait_for_result() -> bool {
    false
}

/// Receiver implementation that receives OTLP HTTP requests and decodes the data into OTAP.
pub struct OTLPHttpReceiver {
    config: Config,
    metrics: MetricSet<OtlpHttpReceiverMetrics>,
}

/// State shared between HTTP server task and the effect handler.
struct SharedStates {
    logs: Option<SharedState>,
    metrics: Option<SharedState>,
    traces: Option<SharedState>,
}

/// Declares the OTLP HTTP receiver as a shared receiver factory
#[allow(unsafe_code)]
#[distributed_slice(OTAP_RECEIVER_FACTORIES)]
pub static OTLP_HTTP_RECEIVER: ReceiverFactory<OtapPdata> = ReceiverFactory {
    name: OTLP_HTTP_RECEIVER_URN,
    create: |pipeline: PipelineContext,
             node: NodeId,
             node_config: Arc<NodeUserConfig>,
             receiver_config: &ReceiverConfig| {
        Ok(ReceiverWrapper::shared(
            OTLPHttpReceiver::from_config(pipeline, &node_config.config)?,
            node,
            node_config,
            receiver_config,
        ))
    },
};

impl OTLPHttpReceiver {
    /// Creates a new OTLPHttpReceiver from a configuration object
    pub fn from_config(
        pipeline_ctx: PipelineContext,
        config: &Value,
    ) -> Result<Self, otap_df_config::error::Error> {
        let config: Config = serde_json::from_value(config.clone()).map_err(|e| {
            otap_df_config::error::Error::InvalidUserConfig {
                error: e.to_string(),
            }
        })?;

        let metrics = pipeline_ctx.register_metrics::<OtlpHttpReceiverMetrics>();

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

/// OTLP HTTP receiver metrics.
#[metric_set(name = "otlp.http.receiver.metrics")]
#[derive(Debug, Default, Clone)]
pub struct OtlpHttpReceiverMetrics {
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

#[derive(Clone)]
struct AppState {
    effect_handler: shared::EffectHandler<OtapPdata>,
    logs_state: Option<SharedState>,
    metrics_state: Option<SharedState>,
    traces_state: Option<SharedState>,
    timeout: Option<Duration>,
}

#[async_trait]
impl shared::Receiver<OtapPdata> for OTLPHttpReceiver {
    async fn start(
        mut self: Box<Self>,
        mut ctrl_msg_recv: shared::ControlChannel<OtapPdata>,
        effect_handler: shared::EffectHandler<OtapPdata>,
    ) -> Result<TerminalState, Error> {
        let logs_state = self
            .config
            .wait_for_result
            .then(|| SharedState::new(self.config.max_concurrent_requests));
        let metrics_state = self
            .config
            .wait_for_result
            .then(|| SharedState::new(self.config.max_concurrent_requests));
        let traces_state = self
            .config
            .wait_for_result
            .then(|| SharedState::new(self.config.max_concurrent_requests));

        let states = SharedStates {
            logs: logs_state.clone(),
            metrics: metrics_state.clone(),
            traces: traces_state.clone(),
        };

        let app_state = AppState {
            effect_handler: effect_handler.clone(),
            logs_state,
            metrics_state,
            traces_state,
            timeout: self.config.timeout,
        };

        let app = Router::new()
            .route("/v1/logs", post(handle_logs))
            .route("/v1/metrics", post(handle_metrics))
            .route("/v1/traces", post(handle_traces))
            .with_state(app_state);

        let listener = tokio::net::TcpListener::bind(self.config.listening_addr)
            .await
            .map_err(|e| Error::ReceiverError {
                receiver: effect_handler.receiver_id(),
                kind: ReceiverErrorKind::Transport,
                error: e.to_string(),
                source_detail: format_error_sources(&e),
            })?;

        // Start periodic telemetry collection
        let telemetry_cancel_handle = effect_handler
            .start_periodic_telemetry(Duration::from_secs(1))
            .await?;

        tokio::select! {
            biased;

            ctrl_msg_result = async {
                loop {
                    match ctrl_msg_recv.recv().await {
                        Ok(NodeControlMsg::Shutdown { deadline, .. }) => {
                            let snapshot = self.metrics.snapshot();
                            _ = telemetry_cancel_handle.cancel().await;
                            return Ok(TerminalState::new(deadline, [snapshot]));
                        },
                        Ok(NodeControlMsg::CollectTelemetry { mut metrics_reporter }) => {
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
                        _ => {}
                    }
                }
            } => {
                return ctrl_msg_result;
            },

            result = axum::serve(listener, app) => {
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

async fn handle_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !is_protobuf_content_type(&headers) {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "Unsupported Content-Type").into_response();
    }

    let otlp_bytes = OtlpProtoBytes::ExportLogsRequest(body);
    let mut otap_pdata = OtapPdata::new(Context::default(), otlp_bytes.into());

    process_request(
        state.effect_handler,
        state.logs_state,
        state.timeout,
        &mut otap_pdata,
        SignalType::Logs,
    )
    .await
}

async fn handle_metrics(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !is_protobuf_content_type(&headers) {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "Unsupported Content-Type").into_response();
    }

    let otlp_bytes = OtlpProtoBytes::ExportMetricsRequest(body);
    let mut otap_pdata = OtapPdata::new(Context::default(), otlp_bytes.into());

    process_request(
        state.effect_handler,
        state.metrics_state,
        state.timeout,
        &mut otap_pdata,
        SignalType::Metrics,
    )
    .await
}

async fn handle_traces(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !is_protobuf_content_type(&headers) {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "Unsupported Content-Type").into_response();
    }

    let otlp_bytes = OtlpProtoBytes::ExportTracesRequest(body);
    let mut otap_pdata = OtapPdata::new(Context::default(), otlp_bytes.into());

    process_request(
        state.effect_handler,
        state.traces_state,
        state.timeout,
        &mut otap_pdata,
        SignalType::Traces,
    )
    .await
}

fn is_protobuf_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("application/x-protobuf"))
        .unwrap_or(false)
}

async fn process_request(
    effect_handler: shared::EffectHandler<OtapPdata>,
    state: Option<SharedState>,
    timeout_duration: Option<Duration>,
    otap_pdata: &mut OtapPdata,
    signal_type: SignalType,
) -> Response {
    let cancel_rx = if let Some(state) = state {
        let (key, rx) = match state
            .0
            .lock()
            .map(|mut state| state.allocate(|| oneshot::channel()))
        {
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "Mutex poisoned").into_response(),
            Ok(None) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Too many concurrent requests",
                )
                    .into_response();
            }
            Ok(Some(pair)) => pair,
        };

        effect_handler.subscribe_to(
            Interests::ACKS | Interests::NACKS,
            key.into(),
            otap_pdata,
        );
        Some((SlotGuard { key, state }, rx))
    } else {
        None
    };

    match effect_handler.send_message(otap_pdata.clone()).await {
        Ok(_) => {}
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to send to pipeline: {e}"),
            )
                .into_response();
        }
    };

    if let Some((_cancel_guard, rx)) = cancel_rx {
        let result = if let Some(duration) = timeout_duration {
            match tokio::time::timeout(duration, rx).await {
                Ok(res) => res,
                Err(_) => {
                    return (StatusCode::REQUEST_TIMEOUT, "Request timed out").into_response();
                }
            }
        } else {
            rx.await
        };

        match result {
            Ok(Ok(())) => {}
            Ok(Err(nack)) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Pipeline processing failed: {}", nack.reason),
                )
                    .into_response();
            }
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Response channel closed unexpectedly",
                )
                    .into_response();
            }
        }
    }

    // Encode response
    let mut buf = Vec::new();
    match signal_type {
        SignalType::Logs => {
            let response = ExportLogsServiceResponse {
                partial_success: None,
            };
            response.encode(&mut buf).unwrap();
        }
        SignalType::Metrics => {
            let response = ExportMetricsServiceResponse {
                partial_success: None,
            };
            response.encode(&mut buf).unwrap();
        }
        SignalType::Traces => {
            let response = ExportTraceServiceResponse {
                partial_success: None,
            };
            response.encode(&mut buf).unwrap();
        }
    }

    (
        StatusCode::OK,
        [(http::header::CONTENT_TYPE, "application/x-protobuf")],
        buf,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use otap_df_config::node::NodeUserConfig;
    use otap_df_engine::context::ControllerContext;
    use otap_df_engine::control::{AckMsg, NodeControlMsg};
    use otap_df_engine::receiver::ReceiverWrapper;
    use otap_df_engine::testing::{
        receiver::{NotSendValidateContext, TestContext, TestRuntime},
        test_node,
    };
    use otap_df_pdata::OtlpProtoBytes;
    use otap_df_pdata::proto::opentelemetry::collector::logs::v1::ExportLogsServiceRequest;
    use otap_df_pdata::proto::opentelemetry::collector::metrics::v1::ExportMetricsServiceRequest;
    use otap_df_pdata::proto::opentelemetry::collector::trace::v1::ExportTraceServiceRequest;
    use otap_df_pdata::proto::opentelemetry::common::v1::{InstrumentationScope, KeyValue};
    use otap_df_pdata::proto::opentelemetry::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use otap_df_pdata::proto::opentelemetry::metrics::v1::{ResourceMetrics, ScopeMetrics};
    use otap_df_pdata::proto::opentelemetry::resource::v1::Resource;
    use otap_df_pdata::proto::opentelemetry::trace::v1::{ResourceSpans, ScopeSpans};
    use otap_df_telemetry::registry::MetricsRegistryHandle;
    use prost::Message;
    use std::future::Future;
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

    fn scenario(
        http_endpoint: String,
    ) -> impl FnOnce(TestContext<OtapPdata>) -> Pin<Box<dyn Future<Output = ()>>> {
        move |ctx| {
            Box::pin(async move {
                let client = reqwest::Client::new();

                // Send logs
                let logs_req = create_logs_service_request();
                let mut buf = Vec::new();
                logs_req.encode(&mut buf).unwrap();
                let resp = client
                    .post(format!("{}/v1/logs", http_endpoint))
                    .header(http::header::CONTENT_TYPE, "application/x-protobuf")
                    .body(buf)
                    .send()
                    .await
                    .expect("Failed to send logs request");
                assert_eq!(resp.status(), StatusCode::OK);

                // Send metrics
                let metrics_req = create_metrics_service_request();
                let mut buf = Vec::new();
                metrics_req.encode(&mut buf).unwrap();
                let resp = client
                    .post(format!("{}/v1/metrics", http_endpoint))
                    .header(http::header::CONTENT_TYPE, "application/x-protobuf")
                    .body(buf)
                    .send()
                    .await
                    .expect("Failed to send metrics request");
                assert_eq!(resp.status(), StatusCode::OK);

                // Send traces
                let traces_req = create_traces_service_request();
                let mut buf = Vec::new();
                traces_req.encode(&mut buf).unwrap();
                let resp = client
                    .post(format!("{}/v1/traces", http_endpoint))
                    .header(http::header::CONTENT_TYPE, "application/x-protobuf")
                    .body(buf)
                    .send()
                    .await
                    .expect("Failed to send traces request");
                assert_eq!(resp.status(), StatusCode::OK);

                ctx.send_shutdown(Instant::now(), "Test")
                    .await
                    .expect("Failed to send Shutdown");
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

                // Send Ack
                if let Some((_node_id, ack)) =
                    Context::next_ack(AckMsg::new(logs_pdata))
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

                // Send Ack
                if let Some((_node_id, ack)) =
                    Context::next_ack(AckMsg::new(metrics_pdata))
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

                // Send Ack
                if let Some((_node_id, ack)) =
                    Context::next_ack(AckMsg::new(trace_pdata))
                {
                    ctx.send_control_msg(NodeControlMsg::Ack(ack))
                        .await
                        .expect("Failed to send Ack for traces");
                }
            })
        }
    }

    #[test]
    fn test_otlp_http_receiver() {
        let test_runtime = TestRuntime::new();

        let http_addr = "127.0.0.1";
        let http_port = portpicker::pick_unused_port().expect("No free ports");
        let http_endpoint = format!("http://{http_addr}:{http_port}");
        let addr: SocketAddr = format!("{http_addr}:{http_port}").parse().unwrap();

        let node_config = Arc::new(NodeUserConfig::new_receiver_config(OTLP_HTTP_RECEIVER_URN));

        let metrics_registry_handle = MetricsRegistryHandle::new();
        let controller_ctx = ControllerContext::new(metrics_registry_handle);
        let pipeline_ctx =
            controller_ctx.pipeline_context_with("grp".into(), "pipeline".into(), 0, 0);

        let receiver = ReceiverWrapper::shared(
            OTLPHttpReceiver {
                config: Config {
                    wait_for_result: true,
                    listening_addr: addr,
                    max_concurrent_requests: 1000,
                    timeout: None,
                },
                metrics: pipeline_ctx.register_metrics::<OtlpHttpReceiverMetrics>(),
            },
            test_node(test_runtime.config().name.clone()),
            node_config,
            test_runtime.config(),
        );

        test_runtime
            .set_receiver(receiver)
            .run_test(scenario(http_endpoint))
            .run_validation_concurrent(validation_procedure());
    }
}
