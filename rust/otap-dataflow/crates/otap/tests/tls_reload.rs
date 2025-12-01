//! Test for TLS reloading.

use serde_json::json;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tokio::time::timeout;

use otap_df_config::node::NodeUserConfig;
use otap_df_engine::context::ControllerContext;
use otap_df_engine::receiver::ReceiverWrapper;
use otap_df_engine::testing::{
    receiver::{NotSendValidateContext, TestContext, TestRuntime},
    test_node,
};
use otap_df_otap::pdata::OtapPdata;
use otap_df_telemetry::registry::MetricsRegistryHandle;

// OTLP imports
use otap_df_otap::otlp_receiver::{OTLP_RECEIVER_URN, OTLPReceiver};
use otap_df_pdata::proto::opentelemetry::collector::logs::v1::ExportLogsServiceRequest;
use otap_df_pdata::proto::opentelemetry::collector::logs::v1::logs_service_client::LogsServiceClient;

// OTAP imports
use arrow::array::{RecordBatch, UInt16Array};
use arrow::datatypes::{DataType, Field, Schema};
use async_stream::stream;
use otap_df_otap::otap_receiver::{OTAP_RECEIVER_URN as OTAP_URN, OTAPReceiver};
use otap_df_pdata::Producer;
use otap_df_pdata::otap::{Logs, Metrics, OtapArrowRecords, Traces};
use otap_df_pdata::proto::opentelemetry::arrow::v1::ArrowPayloadType;
use otap_df_pdata::proto::opentelemetry::arrow::v1::arrow_logs_service_client::ArrowLogsServiceClient;
use otap_df_pdata::schema::consts;

use otap_df_otap::testing::{generate_ca, generate_server_cert};

fn create_otap_batch(batch_id: u64, payload_type: ArrowPayloadType) -> OtapArrowRecords {
    let record_batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new(
            consts::ID,
            DataType::UInt16,
            true,
        )])),
        vec![Arc::new(UInt16Array::from_iter_values(vec![
            batch_id as u16,
        ]))],
    )
    .expect("Failed to create test record batch");

    let mut otap_batch = match payload_type {
        ArrowPayloadType::Logs => OtapArrowRecords::Logs(Logs::default()),
        ArrowPayloadType::Spans => OtapArrowRecords::Traces(Traces::default()),
        ArrowPayloadType::UnivariateMetrics | ArrowPayloadType::MultivariateMetrics => {
            OtapArrowRecords::Metrics(Metrics::default())
        }
        _ => {
            panic!("unexpected payload_type")
        }
    };

    otap_batch.set(payload_type, record_batch);

    otap_batch
}

#[test]
fn test_otlp_receiver_tls_reload() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let test_runtime = TestRuntime::new();

    let grpc_addr = "127.0.0.1";
    let grpc_port = portpicker::pick_unused_port().expect("No free ports");
    let grpc_endpoint = format!("https://{grpc_addr}:{grpc_port}");
    let addr: SocketAddr = format!("{grpc_addr}:{grpc_port}")
        .parse()
        .expect("Failed to parse socket address");

    let node_config = Arc::new(NodeUserConfig::new_receiver_config(OTLP_RECEIVER_URN));

    let metrics_registry_handle = MetricsRegistryHandle::new();
    let controller_ctx = ControllerContext::new(metrics_registry_handle);
    let pipeline_ctx = controller_ctx.pipeline_context_with("grp".into(), "pipeline".into(), 0, 0);

    // Generate certs in a temp dir
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    generate_ca(temp_dir.path());
    generate_server_cert(temp_dir.path(), "localhost");

    let cert_path = temp_dir.path().join("server.crt");
    let key_path = temp_dir.path().join("server.key");
    let ca_path = temp_dir.path().join("ca.crt");

    // Read CA pem for client config
    let ca_pem = std::fs::read_to_string(&ca_path).expect("failed to read ca cert");

    let config = json!({
        "listening_addr": addr.to_string(),
        "tls": {
            "cert_file": cert_path,
            "key_file": key_path,
            "reload_interval": "1s"
        }
    });

    let receiver = ReceiverWrapper::shared(
        OTLPReceiver::from_config(pipeline_ctx, &config)
            .expect("Failed to create OTLP receiver from config"),
        test_node(test_runtime.config().name.clone()),
        node_config,
        test_runtime.config(),
    );

    let reload_scenario = move |ctx: TestContext<OtapPdata>| {
        Box::pin(async move {
            let ca_cert = tonic::transport::Certificate::from_pem(&ca_pem);
            let tls_config = tonic::transport::ClientTlsConfig::new()
                .ca_certificate(ca_cert.clone())
                .domain_name("localhost");

            // 1. Connect and verify initial cert
            let channel = tonic::transport::Channel::from_shared(grpc_endpoint.clone())
                .expect("Invalid URI")
                .tls_config(tls_config.clone())
                .expect("Failed to configure TLS")
                .connect()
                .await
                .expect("Failed to connect initially");

            let mut logs_client = LogsServiceClient::new(channel);
            let request = ExportLogsServiceRequest::default();
            let _ = logs_client
                .export(request.clone())
                .await
                .expect("Initial request failed");

            // 2. Rotate cert with different CN to test reload
            // Wait for at least one reload interval (1s) to pass before modifying the file.
            // This ensures we don't hit a race where the reloader reads the file *while* we are writing it,
            // or reads it just before we write it and then sleeps for another interval.
            sleep(Duration::from_secs(2)).await;

            let dir_path = temp_dir.path().to_path_buf();

            // Regenerate cert with different CN to verify reload worked
            {
                let dir_path = dir_path.clone();
                tokio::task::spawn_blocking(move || {
                    generate_server_cert(&dir_path, "otherhost");
                })
                .await
                .expect("Failed to regenerate cert");
            }
            println!("Regenerated cert with CN=otherhost");

            // Give the server time to detect and reload the new certificate
            // (reload_interval is 1s, we wait 2s to be safe)
            sleep(Duration::from_secs(2)).await;

            // Client expecting "otherhost"
            let tls_config_new = tonic::transport::ClientTlsConfig::new()
                .ca_certificate(ca_cert)
                .domain_name("otherhost");

            let channel_result = timeout(
                Duration::from_secs(5),
                tonic::transport::Channel::from_shared(grpc_endpoint.clone())
                    .expect("Invalid URI")
                    .tls_config(tls_config_new)
                    .expect("Failed to configure TLS")
                    .connect(),
            )
            .await;

            match channel_result {
                Ok(Ok(channel)) => {
                    println!("Connected successfully with new cert!");
                    let mut logs_client = LogsServiceClient::new(channel);
                    let _ = logs_client
                        .export(request)
                        .await
                        .expect("Request with new cert failed");
                }
                Ok(Err(e)) => {
                    panic!("Failed to connect with new cert (connect error): {}", e);
                }
                Err(_) => {
                    panic!("Timed out connecting with new cert");
                }
            }

            ctx.send_shutdown(Instant::now(), "Test")
                .await
                .expect("Failed to send Shutdown");
        }) as Pin<Box<dyn Future<Output = ()>>>
    };

    let validation = |mut ctx: NotSendValidateContext<OtapPdata>| {
        Box::pin(async move {
            // Just drain messages until channel closed
            while ctx.recv().await.is_ok() {
                // ignore
            }
        }) as Pin<Box<dyn Future<Output = ()>>>
    };

    test_runtime
        .set_receiver(receiver)
        .run_test(reload_scenario)
        .run_validation_concurrent(validation);
}

#[test]
fn test_otap_receiver_tls_reload() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let test_runtime = TestRuntime::new();

    let grpc_addr = "127.0.0.1";
    let grpc_port = portpicker::pick_unused_port().expect("No free ports");
    let grpc_endpoint = format!("https://{grpc_addr}:{grpc_port}");
    let addr: SocketAddr = format!("{grpc_addr}:{grpc_port}")
        .parse()
        .expect("Failed to parse socket address");

    let node_config = Arc::new(NodeUserConfig::new_receiver_config(OTAP_URN));

    let metrics_registry_handle = MetricsRegistryHandle::new();
    let controller_ctx = ControllerContext::new(metrics_registry_handle);
    let pipeline_ctx = controller_ctx.pipeline_context_with("grp".into(), "pipeline".into(), 0, 0);

    // Generate certs in a temp dir
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    generate_ca(temp_dir.path());
    generate_server_cert(temp_dir.path(), "localhost");

    let cert_path = temp_dir.path().join("server.crt");
    let key_path = temp_dir.path().join("server.key");
    let ca_path = temp_dir.path().join("ca.crt");

    // Read CA pem for client config
    let ca_pem = std::fs::read_to_string(&ca_path).expect("failed to read ca cert");

    let config = json!({
        "listening_addr": addr.to_string(),
        "response_stream_channel_size": 100,
        "tls": {
            "cert_file": cert_path,
            "key_file": key_path,
            "reload_interval": "1s"
        }
    });

    let receiver = ReceiverWrapper::shared(
        OTAPReceiver::from_config(pipeline_ctx, &config)
            .expect("Failed to create OTAP receiver from config"),
        test_node(test_runtime.config().name.clone()),
        node_config,
        test_runtime.config(),
    );

    let reload_scenario = move |ctx: TestContext<OtapPdata>| {
        Box::pin(async move {
            let ca_cert = tonic::transport::Certificate::from_pem(&ca_pem);
            let tls_config = tonic::transport::ClientTlsConfig::new()
                .ca_certificate(ca_cert.clone())
                .domain_name("localhost");

            // 1. Connect and verify initial cert
            let channel = tonic::transport::Channel::from_shared(grpc_endpoint.clone())
                .expect("Invalid URI")
                .tls_config(tls_config.clone())
                .expect("Failed to configure TLS")
                .connect()
                .await
                .expect("Failed to connect initially");

            let mut logs_client = ArrowLogsServiceClient::new(channel);

            // Send initial request
            let logs_stream = stream! {
                let mut producer = Producer::new();
                let mut logs_records = create_otap_batch(0, ArrowPayloadType::Logs);
                let bar = producer
                    .produce_bar(&mut logs_records)
                    .expect("Failed to produce test batch");
                yield bar;
            };
            let _ = logs_client
                .arrow_logs(logs_stream)
                .await
                .expect("Initial request failed");

            // 2. Rotate cert
            // Wait for at least one reload interval (1s) to pass before modifying the file.
            // This ensures we don't hit a race where the reloader reads the file *while* we are writing it,
            // or reads it just before we write it and then sleeps for another interval.
            sleep(Duration::from_secs(2)).await;

            let dir_path = temp_dir.path().to_path_buf();

            // Regenerate cert with different CN
            {
                let dir_path = dir_path.clone();
                tokio::task::spawn_blocking(move || {
                    generate_server_cert(&dir_path, "otherhost");
                })
                .await
                .expect("Failed to regenerate cert");
            }
            println!("Regenerated cert with CN=otherhost");

            // 3. Connect again with new cert expectation
            // Wait for the reload interval (1s) to trigger a reload of the new certificate.
            // We wait 3s to be safe and avoid flakiness in CI.
            sleep(Duration::from_secs(3)).await;

            // Client expecting "otherhost"
            let tls_config_new = tonic::transport::ClientTlsConfig::new()
                .ca_certificate(ca_cert)
                .domain_name("otherhost");

            let channel_result = timeout(
                Duration::from_secs(5),
                tonic::transport::Channel::from_shared(grpc_endpoint.clone())
                    .expect("Invalid URI")
                    .tls_config(tls_config_new)
                    .expect("Failed to configure TLS")
                    .connect(),
            )
            .await;

            match channel_result {
                Ok(Ok(channel)) => {
                    println!("Connected successfully with new cert!");
                    let mut logs_client = ArrowLogsServiceClient::new(channel);
                    let logs_stream = stream! {
                        let mut producer = Producer::new();
                        let mut logs_records = create_otap_batch(1, ArrowPayloadType::Logs);
                        let bar = producer
                    .produce_bar(&mut logs_records)
                    .expect("Failed to produce test batch");
                        yield bar;
                    };
                    let _ = logs_client
                        .arrow_logs(logs_stream)
                        .await
                        .expect("Request with new cert failed");
                }
                Ok(Err(e)) => {
                    panic!("Failed to connect with new cert (connect error): {}", e);
                }
                Err(_) => {
                    panic!("Timed out connecting with new cert");
                }
            }

            ctx.send_shutdown(Instant::now(), "Test")
                .await
                .expect("Failed to send Shutdown");
        }) as Pin<Box<dyn Future<Output = ()>>>
    };

    let validation = |mut ctx: NotSendValidateContext<OtapPdata>| {
        Box::pin(async move {
            // Just drain messages until channel closed
            while ctx.recv().await.is_ok() {
                // ignore
            }
        }) as Pin<Box<dyn Future<Output = ()>>>
    };

    test_runtime
        .set_receiver(receiver)
        .run_test(reload_scenario)
        .run_validation_concurrent(validation);
}
