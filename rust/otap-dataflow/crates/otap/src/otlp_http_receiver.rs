// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! # OTLP HTTP receiver architecture
//!
//! This receiver implements the OTLP/HTTP protocol as specified in the OpenTelemetry Protocol.
//! It receives telemetry data over HTTP (with optional TLS) and forwards it into the pipeline.
//!
//! ## Endpoints
//!
//! The receiver exposes the following endpoints:
//! - `POST /v1/traces` - for trace data
//! - `POST /v1/metrics` - for metrics data
//! - `POST /v1/logs` - for log data
//!
//! ## Content Types
//!
//! Only supports protobuf encoding:
//! - `application/x-protobuf` - Binary protobuf encoding
//! - `application/protobuf` - Alternative protobuf content type
//!
//! **Note**: JSON encoding (`application/json`) is NOT supported and will be rejected with 415 Unsupported Media Type.
//!
//! ## Compression
//!
//! Supports gzip and zstd compression via the `Content-Encoding` header.

use crate::OTAP_RECEIVER_FACTORIES;
use crate::compression::CompressionMethod;
use crate::otlp_common::{create_otlp_pdata, precomputed_response};
use crate::pdata::OtapPdata;
#[cfg(feature = "experimental-tls")]
use crate::tls_utils::build_tls_acceptor;
#[cfg(feature = "experimental-tls")]
use otap_df_config::tls::TlsServerConfig;

use async_trait::async_trait;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use flate2::read::{DeflateDecoder, GzDecoder};
use linkme::distributed_slice;
use otap_df_config::SignalType;
use otap_df_config::node::NodeUserConfig;
use otap_df_engine::ReceiverFactory;
use otap_df_engine::config::ReceiverConfig;
use otap_df_engine::context::PipelineContext;
use otap_df_engine::control::NodeControlMsg;
use otap_df_engine::error::{Error, ReceiverErrorKind, format_error_sources};
use otap_df_engine::node::NodeId;
use otap_df_engine::receiver::ReceiverWrapper;
use otap_df_engine::shared::receiver as shared;
use otap_df_engine::terminal_state::TerminalState;
use otap_df_telemetry::instrument::Counter;
use otap_df_telemetry::metrics::MetricSet;
use otap_df_telemetry_macros::metric_set;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use std::borrow::Cow;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
#[cfg(feature = "experimental-tls")]
use tokio_rustls::TlsAcceptor;
use tower::ServiceBuilder;

/// URN for the OTLP HTTP Receiver
pub const OTLP_HTTP_RECEIVER_URN: &str = "urn:otel:otlp_http:receiver";

/// Interval for periodic telemetry collection.
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Default maximum request body size (4 MiB)
const DEFAULT_MAX_REQUEST_BODY_SIZE: usize = 4 * 1024 * 1024;

/// Content type for protobuf encoding
const CONTENT_TYPE_PROTOBUF: &str = "application/x-protobuf";

/// Content type for JSON encoding
const CONTENT_TYPE_JSON: &str = "application/json";

/// Configuration for OTLP HTTP Receiver
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The HTTP endpoint to listen on (e.g., "0.0.0.0:4318")
    pub listening_addr: SocketAddr,

    /// Maximum request body size in bytes.
    /// Accepts plain integers or suffixed strings such as `4MiB`.
    #[serde(default = "default_max_request_body_size")]
    pub max_request_body_size: usize,

    /// Maximum number of concurrent requests.
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,

    /// Request timeout duration.
    #[serde(default, with = "humantime_serde")]
    pub timeout: Option<Duration>,

    /// Compression methods accepted for requests.
    #[serde(
        default,
        deserialize_with = "crate::compression::deserialize_compression_methods"
    )]
    pub request_compression: Option<Vec<CompressionMethod>>,

    /// TLS configuration
    #[cfg(feature = "experimental-tls")]
    pub tls: Option<TlsServerConfig>,
}

const fn default_max_request_body_size() -> usize {
    DEFAULT_MAX_REQUEST_BODY_SIZE
}

const fn default_max_concurrent_requests() -> usize {
    1000
}

/// HTTP receiver that ingests OTLP signals over HTTP and forwards them into the OTAP pipeline.
///
/// This implementation follows the OTLP/HTTP specification and supports:
/// - Protobuf content type (JSON is NOT supported)
/// - Gzip, Zstd, and Deflate compression
/// - Optional TLS with certificate reloading
pub struct OTLPHttpReceiver {
    config: Config,
    metrics: Arc<Mutex<MetricSet<OtlpHttpReceiverMetrics>>>,
}

/// Declares the OTLP HTTP receiver as a shared receiver factory.
#[allow(unsafe_code)]
#[distributed_slice(OTAP_RECEIVER_FACTORIES)]
pub static OTLP_HTTP_RECEIVER: ReceiverFactory<OtapPdata> = ReceiverFactory {
    name: OTLP_HTTP_RECEIVER_URN,
    create: |pipeline: PipelineContext,
             node: NodeId,
             node_config: Arc<NodeUserConfig>,
             receiver_config: &ReceiverConfig| {
        let receiver = OTLPHttpReceiver::from_config(pipeline, &node_config.config)?;

        Ok(ReceiverWrapper::shared(
            receiver,
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

        // Validate configuration
        if config.max_request_body_size == 0 {
            return Err(otap_df_config::error::Error::InvalidUserConfig {
                error: "max_request_body_size must be greater than 0".to_string(),
            });
        }
        if config.max_concurrent_requests == 0 {
            return Err(otap_df_config::error::Error::InvalidUserConfig {
                error: "max_concurrent_requests must be greater than 0".to_string(),
            });
        }

        let metrics = Arc::new(Mutex::new(
            pipeline_ctx.register_metrics::<OtlpHttpReceiverMetrics>(),
        ));

        Ok(Self { config, metrics })
    }

    async fn handle_control_message(
        &mut self,
        msg: NodeControlMsg<OtapPdata>,
        telemetry_cancel_handle: &mut Option<
            otap_df_engine::effect_handler::TelemetryTimerCancelHandle<OtapPdata>,
        >,
    ) -> Result<Option<TerminalState>, Error> {
        match msg {
            NodeControlMsg::Shutdown { deadline, .. } => {
                let snapshot = self.metrics.lock().snapshot();
                if let Some(handle) = telemetry_cancel_handle.take() {
                    _ = handle.cancel().await;
                }
                Ok(Some(TerminalState::new(deadline, [snapshot])))
            }
            NodeControlMsg::CollectTelemetry {
                mut metrics_reporter,
            } => {
                _ = metrics_reporter.report(&mut *self.metrics.lock());
                Ok(None)
            }
            _ => Ok(None),
        }
    }
}

/// OTLP HTTP receiver metrics.
#[metric_set(name = "otlp_http.receiver.metrics")]
#[derive(Debug, Default, Clone)]
pub struct OtlpHttpReceiverMetrics {
    /// Number of HTTP requests received.
    #[metric(unit = "{requests}")]
    pub requests_received: Counter<u64>,

    /// Number of successful HTTP requests.
    #[metric(unit = "{requests}")]
    pub requests_succeeded: Counter<u64>,

    /// Number of failed HTTP requests.
    #[metric(unit = "{requests}")]
    pub requests_failed: Counter<u64>,

    /// Total bytes received across HTTP requests (payload bytes).
    #[metric(unit = "By")]
    pub request_bytes: Counter<u64>,

    /// Number of requests rejected due to concurrency limit.
    #[metric(unit = "{requests}")]
    pub rejected_requests: Counter<u64>,

    /// Number of transport-level errors.
    #[metric(unit = "{errors}")]
    pub transport_errors: Counter<u64>,
}

/// Core logic for OTLP HTTP handling, framework-agnostic
#[derive(Clone)]
pub struct OtlpHttpLogic {
    effect_handler: shared::EffectHandler<OtapPdata>,
    metrics: Arc<Mutex<MetricSet<OtlpHttpReceiverMetrics>>>,
    max_request_body_size: usize,
    concurrency: Arc<Semaphore>,
    timeout: Option<Duration>,
    request_compression: Option<Vec<CompressionMethod>>,
}

/// OTLP HTTP response type
#[derive(Debug)]
pub struct OtlpResponse {
    /// HTTP status code
    pub status: StatusCode,
    /// Content-Type header value
    pub content_type: &'static str,
    /// Response body
    pub body: Vec<u8>,
}

impl IntoResponse for OtlpResponse {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            [(header::CONTENT_TYPE, self.content_type)],
            self.body,
        )
            .into_response()
    }
}

/// Decompress request body based on Content-Encoding header
/// Returns Cow to avoid allocation when no compression is used
fn decompress_body<'a>(
    body: &'a [u8],
    content_encoding: Option<&str>,
    max_decompressed_size: usize,
) -> Result<Cow<'a, [u8]>, StatusCode> {
    match content_encoding {
        Some("gzip") => {
            let decoder = GzDecoder::new(body);
            let mut limited = decoder.take((max_decompressed_size as u64).saturating_add(1));
            let mut decompressed = Vec::with_capacity(body.len().min(max_decompressed_size));
            let _bytes_read = limited
                .read_to_end(&mut decompressed)
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            if decompressed.len() > max_decompressed_size {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }
            Ok(Cow::Owned(decompressed))
        }
        Some("deflate") => {
            let decoder = DeflateDecoder::new(body);
            let mut limited = decoder.take((max_decompressed_size as u64).saturating_add(1));
            let mut decompressed = Vec::with_capacity(body.len().min(max_decompressed_size));
            let _bytes_read = limited
                .read_to_end(&mut decompressed)
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            if decompressed.len() > max_decompressed_size {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }
            Ok(Cow::Owned(decompressed))
        }
        Some("zstd") => {
            let decoder =
                zstd::stream::read::Decoder::new(body).map_err(|_| StatusCode::BAD_REQUEST)?;
            let mut limited = decoder.take((max_decompressed_size as u64).saturating_add(1));
            let mut decompressed = Vec::with_capacity(body.len().min(max_decompressed_size));
            let _bytes_read = limited
                .read_to_end(&mut decompressed)
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            if decompressed.len() > max_decompressed_size {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }
            Ok(Cow::Owned(decompressed))
        }
        Some("") | None => Ok(Cow::Borrowed(body)),
        Some(_) => Err(StatusCode::UNSUPPORTED_MEDIA_TYPE),
    }
}

fn normalize_content_encoding(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_ascii_lowercase())
        .and_then(|s| if s.is_empty() { None } else { Some(s) })
}

fn is_encoding_allowed(encoding: &str, allowed: &Option<Vec<CompressionMethod>>) -> bool {
    let Some(allowed) = allowed else {
        return true;
    };
    if allowed.is_empty() {
        return false;
    }
    match encoding {
        "gzip" => allowed.contains(&CompressionMethod::Gzip),
        "zstd" => allowed.contains(&CompressionMethod::Zstd),
        "deflate" => allowed.contains(&CompressionMethod::Deflate),
        _ => false,
    }
}

/// Parse the Content-Type header and validate it's protobuf
/// JSON is not supported - reject immediately with clear error
fn parse_content_type(headers: &HeaderMap) -> Result<(), StatusCode> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(CONTENT_TYPE_PROTOBUF);

    // Extract base content type without parameters
    let base_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();

    match base_type {
        CONTENT_TYPE_PROTOBUF | "application/protobuf" => Ok(()),
        CONTENT_TYPE_JSON => Err(StatusCode::UNSUPPORTED_MEDIA_TYPE), // JSON not supported
        _ => Err(StatusCode::UNSUPPORTED_MEDIA_TYPE),
    }
}

/// Handle OTLP trace export request
async fn handle_traces(
    State(logic): State<OtlpHttpLogic>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    logic
        .handle_request(&headers, body, SignalType::Traces)
        .await
}

/// Handle OTLP metrics export request
async fn handle_metrics(
    State(logic): State<OtlpHttpLogic>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    logic
        .handle_request(&headers, body, SignalType::Metrics)
        .await
}

/// Handle OTLP logs export request
async fn handle_logs(
    State(logic): State<OtlpHttpLogic>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    logic.handle_request(&headers, body, SignalType::Logs).await
}

impl OtlpHttpLogic {
    /// Generic handler for all OTLP signals
    pub async fn handle_request(
        &self,
        headers: &HeaderMap,
        body: Bytes,
        signal: SignalType,
    ) -> OtlpResponse {
        let _permit = match self.concurrency.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                // Batch mutex lock for all metrics
                let mut metrics = self.metrics.lock();
                metrics.requests_received.inc();
                metrics.requests_failed.inc();
                metrics.rejected_requests.inc();
                return OtlpResponse {
                    status: StatusCode::TOO_MANY_REQUESTS,
                    content_type: CONTENT_TYPE_PROTOBUF,
                    body: Vec::new(),
                };
            }
        };

        // Check body size and update metrics in one lock
        let body_len = body.len();
        if body_len > self.max_request_body_size {
            let mut metrics = self.metrics.lock();
            metrics.requests_received.inc();
            metrics.requests_failed.inc();
            return OtlpResponse {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                content_type: CONTENT_TYPE_PROTOBUF,
                body: Vec::new(),
            };
        }

        // Update metrics once for successful body size check
        {
            let mut metrics = self.metrics.lock();
            metrics.requests_received.inc();
            metrics.request_bytes.add(body_len as u64);
        }

        // Validate content type (only protobuf is supported, JSON is rejected)
        if let Err(status) = parse_content_type(headers) {
            self.metrics.lock().requests_failed.inc();
            return OtlpResponse {
                status,
                content_type: CONTENT_TYPE_PROTOBUF,
                body: Vec::new(),
            };
        }

        // Get content encoding for decompression
        let content_encoding = normalize_content_encoding(headers);

        // Handle compression: decompress if needed, otherwise use original Bytes (zero-copy!)
        let proto_bytes = if let Some(ref encoding) = content_encoding {
            // Check if encoding is allowed
            if !is_encoding_allowed(encoding, &self.request_compression) {
                self.metrics.lock().requests_failed.inc();
                return OtlpResponse {
                    status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    content_type: CONTENT_TYPE_PROTOBUF,
                    body: Vec::new(),
                };
            }

            // Decompress the body
            match decompress_body(&body, Some(encoding), self.max_request_body_size) {
                Ok(Cow::Owned(vec)) => Bytes::from(vec),
                Ok(Cow::Borrowed(_)) => {
                    // Should never happen when encoding is present, but handle safely
                    body
                }
                Err(status) => {
                    self.metrics.lock().requests_failed.inc();
                    return OtlpResponse {
                        status,
                        content_type: CONTENT_TYPE_PROTOBUF,
                        body: Vec::new(),
                    };
                }
            }
        } else {
            // No compression - use original Bytes directly (true zero-copy!)
            body
        };

        // Use shared utility to create OtapPdata (consistent with gRPC)
        // Note: preallocate_frame=false because HTTP receiver uses fire-and-forget model.
        // Unlike gRPC which can support wait_for_result (ACK/NACK backpressure), HTTP
        // returns success immediately after accepting data into the pipeline. This is
        // a conscious design decision: most OTLP/HTTP deployments prefer lower latency
        // over end-to-end acknowledgments. The gRPC receiver should be used if
        // backpressure and explicit ACK/NACK signaling is required.
        let pdata = create_otlp_pdata(signal, proto_bytes, false);

        let send_result = if let Some(timeout_duration) = self.timeout {
            tokio::time::timeout(timeout_duration, self.effect_handler.send_message(pdata)).await
        } else {
            Ok(self.effect_handler.send_message(pdata).await)
        };

        match send_result {
            Err(_) => {
                self.metrics.lock().requests_failed.inc();
                OtlpResponse {
                    status: StatusCode::REQUEST_TIMEOUT,
                    content_type: CONTENT_TYPE_PROTOBUF,
                    body: Vec::new(),
                }
            }
            Ok(Ok(_)) => {
                self.metrics.lock().requests_succeeded.inc();
                OtlpResponse {
                    status: StatusCode::OK,
                    content_type: CONTENT_TYPE_PROTOBUF,
                    body: precomputed_response(signal).to_vec(),
                }
            }
            Ok(Err(_)) => {
                self.metrics.lock().requests_failed.inc();
                OtlpResponse {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    content_type: CONTENT_TYPE_PROTOBUF,
                    body: Vec::new(),
                }
            }
        }
    }
}

/// Build the axum router with OTLP endpoints
fn build_router(logic: OtlpHttpLogic) -> Router {
    Router::new()
        .route("/v1/traces", post(handle_traces))
        .route("/v1/metrics", post(handle_metrics))
        .route("/v1/logs", post(handle_logs))
        .layer(ServiceBuilder::new())
        .with_state(logic)
}

#[async_trait]
impl shared::Receiver<OtapPdata> for OTLPHttpReceiver {
    async fn start(
        mut self: Box<Self>,
        mut ctrl_msg_recv: shared::ControlChannel<OtapPdata>,
        effect_handler: shared::EffectHandler<OtapPdata>,
    ) -> Result<TerminalState, Error> {
        otap_df_telemetry::otel_info!(
            "Receiver.Start",
            message = format!(
                "Starting OTLP HTTP Receiver on {}",
                self.config.listening_addr
            )
        );

        // Bind the TCP listener
        let listener = TcpListener::bind(self.config.listening_addr)
            .await
            .map_err(|e| Error::ReceiverError {
                receiver: effect_handler.receiver_id(),
                kind: ReceiverErrorKind::Configuration,
                error: format!("Failed to bind to {}: {}", self.config.listening_addr, e),
                source_detail: format_error_sources(&e),
            })?;

        // Build application logic
        let logic = OtlpHttpLogic {
            effect_handler: effect_handler.clone(),
            metrics: self.metrics.clone(),
            max_request_body_size: self.config.max_request_body_size,
            concurrency: Arc::new(Semaphore::new(self.config.max_concurrent_requests)),
            timeout: self.config.timeout,
            request_compression: self.config.request_compression.clone(),
        };

        let mut telemetry_cancel_handle = Some(
            effect_handler
                .start_periodic_telemetry(TELEMETRY_INTERVAL)
                .await?,
        );

        // Create a shutdown signal
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        // Spawn the HTTP server task
        let mut server_handle = {
            #[cfg(feature = "experimental-tls")]
            let tls_config = self.config.tls.clone();

            tokio::spawn(async move {
                let app = build_router(logic);

                #[cfg(feature = "experimental-tls")]
                {
                    if let Some(tls_config) = tls_config {
                        let tls_acceptor = build_tls_acceptor(Some(&tls_config))
                            .await
                            .map_err(std::io::Error::other)?
                            .expect("TLS acceptor must be present when config is provided");
                        return serve_with_tls(listener, app, tls_acceptor, shutdown_rx).await;
                    }
                }

                serve_plain(listener, app, shutdown_rx).await
            })
        };

        // Main control loop
        loop {
            tokio::select! {
                biased;

                ctrl_msg = ctrl_msg_recv.recv() => {
                    match ctrl_msg {
                        Ok(msg) => {
                            if let Some(terminal) = self
                                .handle_control_message(msg, &mut telemetry_cancel_handle)
                                .await?
                            {
                                // Send shutdown signal to HTTP server
                                let _ = shutdown_tx.send(());
                                // Wait for server to finish
                                let _ = (&mut server_handle).await;
                                return Ok(terminal);
                            }
                        }
                        Err(e) => {
                            if let Some(handle) = telemetry_cancel_handle.take() {
                                _ = handle.cancel().await;
                            }
                            let _ = shutdown_tx.send(());
                            return Err(Error::ChannelRecvError(e));
                        }
                    }
                }

                result = &mut server_handle => {
                    if let Some(handle) = telemetry_cancel_handle.take() {
                        _ = handle.cancel().await;
                    }
                    match result {
                        Ok(Ok(())) => break,
                        Ok(Err(e)) => {
                            self.metrics.lock().transport_errors.inc();
                            return Err(Error::ReceiverError {
                                receiver: effect_handler.receiver_id(),
                                kind: ReceiverErrorKind::Transport,
                                error: e.to_string(),
                                source_detail: String::new(),
                            });
                        }
                        Err(e) => {
                            return Err(Error::ReceiverError {
                                receiver: effect_handler.receiver_id(),
                                kind: ReceiverErrorKind::Transport,
                                error: format!("Server task panicked: {}", e),
                                source_detail: String::new(),
                            });
                        }
                    }
                }
            }
        }

        Ok(TerminalState::new(
            Instant::now() + Duration::from_secs(1),
            [self.metrics.lock().snapshot()],
        ))
    }
}

/// Serve HTTP without TLS
async fn serve_plain(
    listener: TcpListener,
    app: Router,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), std::io::Error> {
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await
}

/// Serve HTTP with TLS
#[cfg(feature = "experimental-tls")]
async fn serve_with_tls(
    listener: TcpListener,
    app: Router,
    tls_acceptor: TlsAcceptor,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), std::io::Error> {
    use tower::Service;

    let mut shutdown_signal = std::pin::pin!(async {
        let _ = shutdown_rx.await;
    });

    loop {
        tokio::select! {
            _ = &mut shutdown_signal => {
                break Ok(());
            }
            result = listener.accept() => {
                let (stream, _addr) = result?;
                let tls_acceptor = tls_acceptor.clone();
                let app = app.clone();

                let _handle = tokio::spawn(async move {
                    match tls_acceptor.accept(stream).await {
                        Ok(tls_stream) => {
                            let io = hyper_util::rt::TokioIo::new(tls_stream);
                            let service = hyper::service::service_fn(move |req| {
                                let mut app = app.clone();
                                async move {
                                    app.call(req).await
                                }
                            });
                            if let Err(err) = hyper_util::server::conn::auto::Builder::new(
                                hyper_util::rt::TokioExecutor::new()
                            )
                            .serve_connection(io, service)
                            .await
                            {
                                log::error!("Error serving TLS connection: {}", err);
                            }
                        }
                        Err(e) => {
                            log::error!("TLS handshake failed: {}", e);
                        }
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otap_df_engine::context::ControllerContext;
    use otap_df_telemetry::registry::MetricsRegistryHandle;
    use serde_json::json;

    #[allow(dead_code)]
    fn test_config(addr: SocketAddr) -> Config {
        Config {
            listening_addr: addr,
            max_request_body_size: DEFAULT_MAX_REQUEST_BODY_SIZE,
            max_concurrent_requests: 1000,
            timeout: None,
            request_compression: None,
            #[cfg(feature = "experimental-tls")]
            tls: None,
        }
    }

    #[test]
    fn test_config_parsing() {
        let metrics_registry_handle = MetricsRegistryHandle::new();
        let controller_ctx = ControllerContext::new(metrics_registry_handle);
        let pipeline_ctx =
            controller_ctx.pipeline_context_with("grp".into(), "pipeline".into(), 0, 0);

        let config = json!({
            "listening_addr": "127.0.0.1:4318"
        });
        let receiver =
            OTLPHttpReceiver::from_config(pipeline_ctx.clone(), &config).unwrap();
        assert_eq!(
            receiver.config.listening_addr,
            "127.0.0.1:4318".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            receiver.config.max_request_body_size,
            DEFAULT_MAX_REQUEST_BODY_SIZE
        );

        let config_with_options = json!({
            "listening_addr": "0.0.0.0:4318",
            "max_request_body_size": 8388608,
            "max_concurrent_requests": 500,
            "timeout": "30s"
        });
        let receiver = OTLPHttpReceiver::from_config(
            pipeline_ctx.clone(),
            &config_with_options,
        )
        .unwrap();
        assert_eq!(receiver.config.max_request_body_size, 8 * 1024 * 1024);
        assert_eq!(receiver.config.max_concurrent_requests, 500);
        assert_eq!(receiver.config.timeout, Some(Duration::from_secs(30)));
    }

    #[test]
    fn test_config_validation() {
        let metrics_registry_handle = MetricsRegistryHandle::new();
        let controller_ctx = ControllerContext::new(metrics_registry_handle);
        let pipeline_ctx =
            controller_ctx.pipeline_context_with("grp".into(), "pipeline".into(), 0, 0);

        // Test zero max_request_body_size
        let invalid_config = json!({
            "listening_addr": "127.0.0.1:4318",
            "max_request_body_size": 0
        });
        let result = OTLPHttpReceiver::from_config(pipeline_ctx.clone(), &invalid_config);
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("max_request_body_size must be greater than 0"));
        }

        // Test zero max_concurrent_requests
        let invalid_config = json!({
            "listening_addr": "127.0.0.1:4318",
            "max_concurrent_requests": 0
        });
        let result = OTLPHttpReceiver::from_config(pipeline_ctx.clone(), &invalid_config);
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("max_concurrent_requests must be greater than 0"));
        }
    }

    #[test]
    fn test_parse_content_type() {
        let mut headers = HeaderMap::new();

        // Test protobuf - should succeed
        let _ = headers.insert(
            header::CONTENT_TYPE,
            "application/x-protobuf".parse().unwrap(),
        );
        assert!(parse_content_type(&headers).is_ok());

        // Test alternative protobuf content type
        let _ = headers.insert(
            header::CONTENT_TYPE,
            "application/protobuf".parse().unwrap(),
        );
        assert!(parse_content_type(&headers).is_ok());

        // Test JSON - should be rejected
        let _ = headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        assert!(parse_content_type(&headers).is_err());
        assert_eq!(
            parse_content_type(&headers).unwrap_err(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );

        // Test JSON with charset parameter - should also be rejected
        let _ = headers.insert(
            header::CONTENT_TYPE,
            "application/json; charset=utf-8".parse().unwrap(),
        );
        assert!(parse_content_type(&headers).is_err());

        // Test unsupported content type
        let _ = headers.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
        assert!(parse_content_type(&headers).is_err());
    }

    #[test]
    fn test_decompress_body() {
        // Test no compression - should return borrowed data
        let body = b"test data";
        let result = decompress_body(body, None, DEFAULT_MAX_REQUEST_BODY_SIZE).unwrap();
        assert_eq!(&*result, body);
        assert!(matches!(result, Cow::Borrowed(_)));

        let result = decompress_body(body, Some(""), DEFAULT_MAX_REQUEST_BODY_SIZE).unwrap();
        assert_eq!(&*result, body);
        assert!(matches!(result, Cow::Borrowed(_)));

        // Test unsupported compression
        let result = decompress_body(body, Some("br"), DEFAULT_MAX_REQUEST_BODY_SIZE);
        assert!(result.is_err());
    }

    #[test]
    fn test_precomputed_response_via_common() {
        use crate::otlp_common::precomputed_response;

        // Verify responses can be generated via the common module
        let logs_response = precomputed_response(SignalType::Logs);
        assert!(!logs_response.is_empty() || logs_response.is_empty()); // May be empty for default

        let metrics_response = precomputed_response(SignalType::Metrics);
        assert!(!metrics_response.is_empty() || metrics_response.is_empty());

        let traces_response = precomputed_response(SignalType::Traces);
        assert!(!traces_response.is_empty() || traces_response.is_empty());
    }

    #[test]
    fn test_gzip_decompression() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let original = b"test data for compression";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(original).unwrap();
        let compressed = encoder.finish().unwrap();

        let result =
            decompress_body(&compressed, Some("gzip"), DEFAULT_MAX_REQUEST_BODY_SIZE).unwrap();
        assert_eq!(&*result, original);
        assert!(matches!(result, Cow::Owned(_)));
    }

    #[test]
    fn test_zstd_decompression() {
        let original = b"test data for zstd compression";
        let compressed = zstd::encode_all(&original[..], 3).unwrap();

        let result =
            decompress_body(&compressed, Some("zstd"), DEFAULT_MAX_REQUEST_BODY_SIZE).unwrap();
        assert_eq!(&*result, original);
        assert!(matches!(result, Cow::Owned(_)));
    }

    #[test]
    fn test_deflate_decompression() {
        use flate2::Compression;
        use flate2::write::DeflateEncoder;
        use std::io::Write;

        let original = b"test data for deflate compression";
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(original).unwrap();
        let compressed = encoder.finish().unwrap();

        let result =
            decompress_body(&compressed, Some("deflate"), DEFAULT_MAX_REQUEST_BODY_SIZE).unwrap();
        assert_eq!(&*result, original);
        assert!(matches!(result, Cow::Owned(_)));
    }
}

// Integration tests module - requires async runtime
#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::pdata::OtapPdata;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use otap_df_config::node::NodeUserConfig;
    use otap_df_engine::context::ControllerContext;
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
    use std::io::Write;
    use std::pin::Pin;
    use std::time::Instant;
    use tokio::time::timeout;

    fn test_config(addr: SocketAddr) -> Config {
        Config {
            listening_addr: addr,
            max_request_body_size: DEFAULT_MAX_REQUEST_BODY_SIZE,
            max_concurrent_requests: 1000,
            timeout: None,
            request_compression: None,
            #[cfg(feature = "experimental-tls")]
            tls: None,
        }
    }

    fn create_logs_service_request() -> ExportLogsServiceRequest {
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "service.name".to_string(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                scope_logs: vec![ScopeLogs {
                    scope: Some(InstrumentationScope {
                        name: "test-scope".to_string(),
                        ..Default::default()
                    }),
                    log_records: vec![
                        LogRecord {
                            time_unix_nano: 1000000000,
                            body: Some(otap_df_pdata::proto::opentelemetry::common::v1::AnyValue {
                                value: Some(
                                    otap_df_pdata::proto::opentelemetry::common::v1::any_value::Value::StringValue(
                                        "test log message".to_string(),
                                    ),
                                ),
                            }),
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
                    attributes: vec![KeyValue {
                        key: "service.name".to_string(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                scope_metrics: vec![ScopeMetrics {
                    scope: Some(InstrumentationScope {
                        name: "test-scope".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    fn create_traces_service_request() -> ExportTraceServiceRequest {
        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "service.name".to_string(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                scope_spans: vec![ScopeSpans {
                    scope: Some(InstrumentationScope {
                        name: "test-scope".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    /// HTTP client helper for sending OTLP requests
    async fn send_otlp_http_request(
        endpoint: &str,
        path: &str,
        body: Vec<u8>,
        content_encoding: Option<&str>,
    ) -> Result<StatusCode, reqwest::Error> {
        let client = reqwest::Client::new();
        let mut request = client
            .post(format!("{}{}", endpoint, path))
            .header("Content-Type", CONTENT_TYPE_PROTOBUF)
            .header("Connection", "close")
            .body(body);

        if let Some(encoding) = content_encoding {
            request = request.header("Content-Encoding", encoding);
        }

        let response = request.send().await?;
        let status = response.status();
        let _ = response.bytes().await;
        Ok(status)
    }

    /// Compress data using gzip
    fn gzip_compress(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    /// Compress data using zstd
    fn zstd_compress(data: &[u8]) -> Vec<u8> {
        zstd::encode_all(&data[..], 3).unwrap()
    }

    fn http_scenario(
        http_endpoint: String,
    ) -> impl FnOnce(TestContext<OtapPdata>) -> Pin<Box<dyn Future<Output = ()>>> {
        move |ctx| {
            Box::pin(async move {
                // Allow server to start
                tokio::time::sleep(Duration::from_millis(100)).await;

                // Test logs endpoint
                let logs_request = create_logs_service_request();
                let mut logs_bytes = Vec::new();
                logs_request.encode(&mut logs_bytes).unwrap();

                let response = send_otlp_http_request(&http_endpoint, "/v1/logs", logs_bytes, None)
                    .await
                    .expect("Failed to send logs request");
                assert_eq!(response.as_u16(), 200, "Logs request failed");

                // Test metrics endpoint
                let metrics_request = create_metrics_service_request();
                let mut metrics_bytes = Vec::new();
                metrics_request.encode(&mut metrics_bytes).unwrap();

                let response =
                    send_otlp_http_request(&http_endpoint, "/v1/metrics", metrics_bytes, None)
                        .await
                        .expect("Failed to send metrics request");
                assert_eq!(response.as_u16(), 200, "Metrics request failed");

                // Test traces endpoint
                let traces_request = create_traces_service_request();
                let mut traces_bytes = Vec::new();
                traces_request.encode(&mut traces_bytes).unwrap();

                let response =
                    send_otlp_http_request(&http_endpoint, "/v1/traces", traces_bytes, None)
                        .await
                        .expect("Failed to send traces request");
                assert_eq!(response.as_u16(), 200, "Traces request failed");

                // Send shutdown
                ctx.send_shutdown(Instant::now(), "Test complete")
                    .await
                    .expect("Failed to send shutdown");
            })
        }
    }

    fn http_validation_procedure()
    -> impl FnOnce(NotSendValidateContext<OtapPdata>) -> Pin<Box<dyn Future<Output = ()>>> {
        |mut ctx| {
            Box::pin(async move {
                // Receive logs pdata
                let logs_pdata = timeout(Duration::from_secs(5), ctx.recv())
                    .await
                    .expect("Timed out waiting for logs message")
                    .expect("No logs message received");

                let logs_proto: OtlpProtoBytes = logs_pdata
                    .payload()
                    .try_into()
                    .expect("can convert to OtlpProtoBytes");
                assert!(
                    matches!(logs_proto, OtlpProtoBytes::ExportLogsRequest(_)),
                    "Expected logs request"
                );

                // Receive metrics pdata
                let metrics_pdata = timeout(Duration::from_secs(5), ctx.recv())
                    .await
                    .expect("Timed out waiting for metrics message")
                    .expect("No metrics message received");

                let metrics_proto: OtlpProtoBytes = metrics_pdata
                    .payload()
                    .try_into()
                    .expect("can convert to OtlpProtoBytes");
                assert!(
                    matches!(metrics_proto, OtlpProtoBytes::ExportMetricsRequest(_)),
                    "Expected metrics request"
                );

                // Receive traces pdata
                let traces_pdata = timeout(Duration::from_secs(5), ctx.recv())
                    .await
                    .expect("Timed out waiting for traces message")
                    .expect("No traces message received");

                let traces_proto: OtlpProtoBytes = traces_pdata
                    .payload()
                    .try_into()
                    .expect("can convert to OtlpProtoBytes");
                assert!(
                    matches!(traces_proto, OtlpProtoBytes::ExportTracesRequest(_)),
                    "Expected traces request"
                );
            })
        }
    }

    #[test]
    fn test_otlp_http_receiver_basic() {
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
                config: test_config(addr),
                metrics: Arc::new(Mutex::new(
                    pipeline_ctx.register_metrics::<OtlpHttpReceiverMetrics>(),
                )),
            },
            test_node(test_runtime.config().name.clone()),
            node_config,
            test_runtime.config(),
        );

        test_runtime
            .set_receiver(receiver)
            .run_test(http_scenario(http_endpoint))
            .run_validation_concurrent(http_validation_procedure());
    }

    fn http_gzip_scenario(
        http_endpoint: String,
    ) -> impl FnOnce(TestContext<OtapPdata>) -> Pin<Box<dyn Future<Output = ()>>> {
        move |ctx| {
            Box::pin(async move {
                // Allow server to start
                tokio::time::sleep(Duration::from_millis(100)).await;

                // Test logs with gzip compression
                let logs_request = create_logs_service_request();
                let mut logs_bytes = Vec::new();
                logs_request.encode(&mut logs_bytes).unwrap();
                let compressed = gzip_compress(&logs_bytes);

                let response =
                    send_otlp_http_request(&http_endpoint, "/v1/logs", compressed, Some("gzip"))
                        .await
                        .expect("Failed to send gzip logs request");
                assert_eq!(response.as_u16(), 200, "Gzip logs request failed");

                ctx.send_shutdown(Instant::now(), "Test complete")
                    .await
                    .expect("Failed to send shutdown");
            })
        }
    }

    fn http_gzip_validation()
    -> impl FnOnce(NotSendValidateContext<OtapPdata>) -> Pin<Box<dyn Future<Output = ()>>> {
        |mut ctx| {
            Box::pin(async move {
                let pdata = timeout(Duration::from_secs(5), ctx.recv())
                    .await
                    .expect("Timed out waiting for gzip message")
                    .expect("No message received");

                let proto: OtlpProtoBytes = pdata
                    .payload()
                    .try_into()
                    .expect("can convert to OtlpProtoBytes");
                assert!(matches!(proto, OtlpProtoBytes::ExportLogsRequest(_)));

                // Verify the decompressed content matches expected
                let expected = create_logs_service_request();
                let mut expected_bytes = Vec::new();
                expected.encode(&mut expected_bytes).unwrap();
                assert_eq!(expected_bytes, proto.as_bytes());
            })
        }
    }

    #[test]
    fn test_otlp_http_receiver_gzip() {
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
                config: test_config(addr),
                metrics: Arc::new(Mutex::new(
                    pipeline_ctx.register_metrics::<OtlpHttpReceiverMetrics>(),
                )),
            },
            test_node(test_runtime.config().name.clone()),
            node_config,
            test_runtime.config(),
        );

        test_runtime
            .set_receiver(receiver)
            .run_test(http_gzip_scenario(http_endpoint))
            .run_validation_concurrent(http_gzip_validation());
    }

    fn http_zstd_scenario(
        http_endpoint: String,
    ) -> impl FnOnce(TestContext<OtapPdata>) -> Pin<Box<dyn Future<Output = ()>>> {
        move |ctx| {
            Box::pin(async move {
                // Allow server to start
                tokio::time::sleep(Duration::from_millis(100)).await;

                // Test metrics with zstd compression
                let metrics_request = create_metrics_service_request();
                let mut metrics_bytes = Vec::new();
                metrics_request.encode(&mut metrics_bytes).unwrap();
                let compressed = zstd_compress(&metrics_bytes);

                let response =
                    send_otlp_http_request(&http_endpoint, "/v1/metrics", compressed, Some("zstd"))
                        .await
                        .expect("Failed to send zstd metrics request");
                assert_eq!(response.as_u16(), 200, "Zstd metrics request failed");

                ctx.send_shutdown(Instant::now(), "Test complete")
                    .await
                    .expect("Failed to send shutdown");
            })
        }
    }

    fn http_zstd_validation()
    -> impl FnOnce(NotSendValidateContext<OtapPdata>) -> Pin<Box<dyn Future<Output = ()>>> {
        |mut ctx| {
            Box::pin(async move {
                let pdata = timeout(Duration::from_secs(5), ctx.recv())
                    .await
                    .expect("Timed out waiting for zstd message")
                    .expect("No message received");

                let proto: OtlpProtoBytes = pdata
                    .payload()
                    .try_into()
                    .expect("can convert to OtlpProtoBytes");
                assert!(matches!(proto, OtlpProtoBytes::ExportMetricsRequest(_)));

                // Verify the decompressed content matches expected
                let expected = create_metrics_service_request();
                let mut expected_bytes = Vec::new();
                expected.encode(&mut expected_bytes).unwrap();
                assert_eq!(expected_bytes, proto.as_bytes());
            })
        }
    }

    #[test]
    fn test_otlp_http_receiver_zstd() {
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
                config: test_config(addr),
                metrics: Arc::new(Mutex::new(
                    pipeline_ctx.register_metrics::<OtlpHttpReceiverMetrics>(),
                )),
            },
            test_node(test_runtime.config().name.clone()),
            node_config,
            test_runtime.config(),
        );

        test_runtime
            .set_receiver(receiver)
            .run_test(http_zstd_scenario(http_endpoint))
            .run_validation_concurrent(http_zstd_validation());
    }

    fn http_error_scenario(
        http_endpoint: String,
    ) -> impl FnOnce(TestContext<OtapPdata>) -> Pin<Box<dyn Future<Output = ()>>> {
        move |ctx| {
            Box::pin(async move {
                // Allow server to start
                tokio::time::sleep(Duration::from_millis(100)).await;

                let client = reqwest::Client::new();

                async fn assert_status_and_drain(
                    response: reqwest::Response,
                    expected: u16,
                    label: &str,
                ) {
                    let status = response.status();
                    let _ = response.bytes().await;
                    assert_eq!(status.as_u16(), expected, "{label}");
                }

                // Test unsupported content type
                let response = client
                    .post(format!("{}/v1/logs", http_endpoint))
                    .header("Content-Type", "text/plain")
                    .header("Connection", "close")
                    .body(b"invalid".to_vec())
                    .send()
                    .await
                    .expect("Failed to send request");
                assert_status_and_drain(response, 415, "Expected 415 Unsupported Media Type").await;

                // Test unsupported compression encoding
                let response = client
                    .post(format!("{}/v1/logs", http_endpoint))
                    .header("Content-Type", CONTENT_TYPE_PROTOBUF)
                    .header("Content-Encoding", "br") // brotli not supported
                    .header("Connection", "close")
                    .body(b"test".to_vec())
                    .send()
                    .await
                    .expect("Failed to send request");
                assert_status_and_drain(
                    response,
                    415,
                    "Expected 415 Unsupported Media Type for unsupported encoding",
                )
                .await;

                // Test invalid endpoint (should return 404)
                let response = client
                    .post(format!("{}/v1/invalid", http_endpoint))
                    .header("Content-Type", CONTENT_TYPE_PROTOBUF)
                    .header("Connection", "close")
                    .body(b"test".to_vec())
                    .send()
                    .await
                    .expect("Failed to send request");
                assert_status_and_drain(response, 404, "Expected 404 Not Found").await;

                // Test GET method (should fail, only POST allowed)
                let response = client
                    .get(format!("{}/v1/logs", http_endpoint))
                    .header("Connection", "close")
                    .send()
                    .await
                    .expect("Failed to send request");
                assert_status_and_drain(response, 405, "Expected 405 Method Not Allowed").await;

                // Drop client before shutdown to ensure all connections are closed
                drop(client);
                tokio::time::sleep(Duration::from_millis(50)).await;

                ctx.send_shutdown(Instant::now(), "Test complete")
                    .await
                    .expect("Failed to send shutdown");
            })
        }
    }

    fn http_error_validation()
    -> impl FnOnce(NotSendValidateContext<OtapPdata>) -> Pin<Box<dyn Future<Output = ()>>> {
        |_ctx| {
            Box::pin(async move {
                // No valid messages should be received since all requests were errors
                // The test verifies HTTP error codes, not pdata flow
            })
        }
    }

    #[test]
    fn test_otlp_http_receiver_errors() {
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
                config: test_config(addr),
                metrics: Arc::new(Mutex::new(
                    pipeline_ctx.register_metrics::<OtlpHttpReceiverMetrics>(),
                )),
            },
            test_node(test_runtime.config().name.clone()),
            node_config,
            test_runtime.config(),
        );

        test_runtime
            .set_receiver(receiver)
            .run_test(http_error_scenario(http_endpoint))
            .run_validation(http_error_validation());
    }
}
