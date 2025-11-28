//! Test for TLS reloading.

use std::sync::Arc;
use std::time::{Duration, Instant};
use std::process::Command;
use tokio::time::sleep;
use serde_json::json;
use std::net::SocketAddr;
use std::pin::Pin;
use std::future::Future;
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
use otap_df_otap::otlp_receiver::{OTLPReceiver, OTLP_RECEIVER_URN};
use otap_df_pdata::proto::opentelemetry::collector::logs::v1::logs_service_client::LogsServiceClient;
use otap_df_pdata::proto::opentelemetry::collector::logs::v1::ExportLogsServiceRequest;

// OTAP imports
use otap_df_otap::otap_receiver::{OTAPReceiver, OTAP_RECEIVER_URN as OTAP_URN};
use otap_df_pdata::proto::opentelemetry::arrow::v1::arrow_logs_service_client::ArrowLogsServiceClient;
use async_stream::stream;
use otap_df_pdata::Producer;
use otap_df_pdata::proto::opentelemetry::arrow::v1::ArrowPayloadType;
use arrow::array::{RecordBatch, UInt16Array};
use arrow::datatypes::{DataType, Field, Schema};
use otap_df_pdata::otap::{Logs, Metrics, OtapArrowRecords, Traces};
use otap_df_pdata::schema::consts;

fn pick_unused_port() -> u16 {
    use rand::Rng;
    let mut rng = rand::rng();
    // Try multiple times to find a free port in a safe range.
    // We avoid binding to port 0 as some restricted environments disallow it.
    for _ in 0..50 {
        let port = rng.random_range(20000..60000);
        if std::net::TcpListener::bind(format!("127.0.0.1:{}", port)).is_ok() {
            return port;
        }
    }
    panic!("Failed to find a free port after multiple attempts");
}

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
    .unwrap();

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

fn generate_ca(dir: &std::path::Path) {
    let status = Command::new("openssl")
        .args(&[
            "req", "-x509", "-newkey", "rsa:2048", "-keyout", "ca.key", "-out", "ca.crt",
            "-days", "365", "-nodes", "-subj", "/CN=Test CA",
            "-addext", "basicConstraints=critical,CA:TRUE",
        ])
        .current_dir(dir)
        .output()
        .expect("Failed to generate CA");
    if !status.status.success() {
        panic!("CA gen failed: {}", String::from_utf8_lossy(&status.stderr));
    }
}

fn generate_server_cert(dir: &std::path::Path, cn: &str) {
    // Generate Server Key and CSR
    let status = Command::new("openssl")
        .args(&[
            "req", "-newkey", "rsa:2048", "-keyout", "server.key", "-out", "server.csr",
            "-nodes", "-subj", &format!("/CN={}", cn),
            "-addext", &format!("subjectAltName=DNS:{},IP:127.0.0.1", cn),
        ])
        .current_dir(dir)
        .output()
        .expect("Failed to generate CSR");
    if !status.status.success() {
        panic!("CSR gen failed: {}", String::from_utf8_lossy(&status.stderr));
    }

    // Sign Server CSR with CA
    let status = Command::new("openssl")
        .args(&[
            "x509", "-req", "-in", "server.csr", "-CA", "ca.crt", "-CAkey", "ca.key",
            "-CAcreateserial", "-out", "server.crt", "-days", "365",
            "-copy_extensions", "copy",
        ])
        .current_dir(dir)
        .output()
        .expect("Failed to sign cert");
    if !status.status.success() {
        panic!("Sign failed: {}", String::from_utf8_lossy(&status.stderr));
    }
}

#[test]
fn test_otlp_receiver_tls_reload() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let test_runtime = TestRuntime::new();

    let grpc_addr = "127.0.0.1";
    let grpc_port = pick_unused_port();
    let grpc_endpoint = format!("https://{grpc_addr}:{grpc_port}");
    let addr: SocketAddr = format!("{grpc_addr}:{grpc_port}").parse().unwrap();

    let node_config = Arc::new(NodeUserConfig::new_receiver_config(OTLP_RECEIVER_URN));

    let metrics_registry_handle = MetricsRegistryHandle::new();
    let controller_ctx = ControllerContext::new(metrics_registry_handle);
    let pipeline_ctx =
        controller_ctx.pipeline_context_with("grp".into(), "pipeline".into(), 0, 0);

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
        OTLPReceiver::from_config(pipeline_ctx, &config).unwrap(),
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
            let _ = logs_client.export(request.clone()).await.expect("Initial request failed");

            // 2. Rotate cert
            sleep(Duration::from_secs(2)).await;
            
            let dir_path = temp_dir.path().to_path_buf();
            
            // Regenerate cert
            {
                let dir_path = dir_path.clone();
                tokio::task::spawn_blocking(move || {
                    generate_server_cert(&dir_path, "localhost");
                }).await.expect("Failed to regenerate cert");
            }

            // 3. Connect again with new cert expectation
            println!("Connecting with new cert expectation...");
            
            sleep(Duration::from_secs(2)).await; // Give some time for reload to happen

            // Regenerate with different CN
             {
                let dir_path = dir_path.clone();
                tokio::task::spawn_blocking(move || {
                    generate_server_cert(&dir_path, "otherhost");
                }).await.expect("Failed to regenerate cert");
            }
            println!("Regenerated cert with CN=otherhost");
            
            // Client expecting "otherhost"
            let tls_config_new = tonic::transport::ClientTlsConfig::new()
                .ca_certificate(ca_cert)
                .domain_name("otherhost");

            let channel_result = timeout(Duration::from_secs(5), tonic::transport::Channel::from_shared(grpc_endpoint.clone())
                .expect("Invalid URI")
                .tls_config(tls_config_new)
                .expect("Failed to configure TLS")
                .connect())
                .await;

            match channel_result {
                Ok(Ok(channel)) => {
                     println!("Connected successfully with new cert!");
                     let mut logs_client = LogsServiceClient::new(channel);
                     let _ = logs_client.export(request).await.expect("Request with new cert failed");
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
            while let Ok(_) = ctx.recv().await {
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
    let grpc_port = pick_unused_port();
    let grpc_endpoint = format!("https://{grpc_addr}:{grpc_port}");
    let addr: SocketAddr = format!("{grpc_addr}:{grpc_port}").parse().unwrap();

    let node_config = Arc::new(NodeUserConfig::new_receiver_config(OTAP_URN));

    let metrics_registry_handle = MetricsRegistryHandle::new();
    let controller_ctx = ControllerContext::new(metrics_registry_handle);
    let pipeline_ctx =
        controller_ctx.pipeline_context_with("grp".into(), "pipeline".into(), 0, 0);

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
        OTAPReceiver::from_config(pipeline_ctx, &config).unwrap(),
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
                let bar = producer.produce_bar(&mut logs_records).unwrap();
                yield bar;
            };
            let _ = logs_client.arrow_logs(logs_stream).await.expect("Initial request failed");

            // 2. Rotate cert
            sleep(Duration::from_secs(2)).await;
            
            let dir_path = temp_dir.path().to_path_buf();
            
            // Regenerate cert with different CN
             {
                let dir_path = dir_path.clone();
                tokio::task::spawn_blocking(move || {
                    generate_server_cert(&dir_path, "otherhost");
                }).await.expect("Failed to regenerate cert");
            }
            println!("Regenerated cert with CN=otherhost");
            
            // 3. Connect again with new cert expectation
            sleep(Duration::from_secs(2)).await; // Give some time for reload to happen

            // Client expecting "otherhost"
            let tls_config_new = tonic::transport::ClientTlsConfig::new()
                .ca_certificate(ca_cert)
                .domain_name("otherhost");

            let channel_result = timeout(Duration::from_secs(5), tonic::transport::Channel::from_shared(grpc_endpoint.clone())
                .expect("Invalid URI")
                .tls_config(tls_config_new)
                .expect("Failed to configure TLS")
                .connect())
                .await;

            match channel_result {
                Ok(Ok(channel)) => {
                     println!("Connected successfully with new cert!");
                     let mut logs_client = ArrowLogsServiceClient::new(channel);
                     let logs_stream = stream! {
                        let mut producer = Producer::new();
                        let mut logs_records = create_otap_batch(1, ArrowPayloadType::Logs);
                        let bar = producer.produce_bar(&mut logs_records).unwrap();
                        yield bar;
                    };
                     let _ = logs_client.arrow_logs(logs_stream).await.expect("Request with new cert failed");
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
            while let Ok(_) = ctx.recv().await {
                // ignore
            }
        }) as Pin<Box<dyn Future<Output = ()>>>
    };

    test_runtime
        .set_receiver(receiver)
        .run_test(reload_scenario)
        .run_validation_concurrent(validation);
}
