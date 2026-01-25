// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! End-to-end integration tests for TLS/mTLS with OTLP and OTAP receivers.
//!
//! These tests use a table-driven approach similar to the Go collector's test patterns.
//! Test scenarios include:
//! - TLS with server certificate verification
//! - mTLS with mutual client/server authentication
//! - Various failure scenarios (wrong CA, missing certs, hostname mismatch, etc.)


#![allow(missing_docs)]

use bytes::Bytes;
use otap_df_config::tls::{TlsClientConfig, TlsConfig, TlsServerConfig};
use otap_df_otap::otap_grpc::client_settings::GrpcClientSettings;
use otap_df_otap::otap_grpc::otlp::client::LogsServiceClient;
use otap_df_otap::otlp_http::HttpServerSettings;
use otap_df_pdata::proto::opentelemetry::collector::logs::v1::ExportLogsServiceRequest;
use prost::Message;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use std::fs;
use std::net::SocketAddr;
use std::time::Duration;
use tempfile::TempDir;

// ============================================================================
// Certificate Generation Helpers
// ============================================================================

/// A complete set of TLS test certificates
struct TestCerts {
    ca_pem: String,
    server_cert_pem: String,
    server_key_pem: String,
    client_cert_pem: String,
    client_key_pem: String,
    /// A second CA for testing "wrong CA" scenarios
    wrong_ca_pem: String,
    /// Client cert signed by wrong CA
    wrong_client_cert_pem: String,
    wrong_client_key_pem: String,
}

impl TestCerts {
    /// Generate a complete set of test certificates
    fn generate() -> Self {
        // Primary CA and certificates
        let (ca, ca_issuer) = new_ca("Test CA");
        let ca_pem = ca.pem();

        let (server_cert_pem, server_key_pem) = new_leaf(
            "localhost",
            "localhost",
            ExtendedKeyUsagePurpose::ServerAuth,
            &ca_issuer,
        );

        let (client_cert_pem, client_key_pem) = new_leaf(
            "client",
            "client",
            ExtendedKeyUsagePurpose::ClientAuth,
            &ca_issuer,
        );

        // Second (wrong) CA for testing untrusted certificates
        let (wrong_ca, wrong_ca_issuer) = new_ca("Wrong CA");
        let wrong_ca_pem = wrong_ca.pem();

        let (wrong_client_cert_pem, wrong_client_key_pem) = new_leaf(
            "wrong-client",
            "wrong-client",
            ExtendedKeyUsagePurpose::ClientAuth,
            &wrong_ca_issuer,
        );

        Self {
            ca_pem,
            server_cert_pem,
            server_key_pem,
            client_cert_pem,
            client_key_pem,
            wrong_ca_pem,
            wrong_client_cert_pem,
            wrong_client_key_pem,
        }
    }

    /// Write certificates to a temp directory and return paths
    fn write_to_dir(&self, dir: &std::path::Path) -> TestCertPaths {
        let ca_path = dir.join("ca.crt");
        let server_cert_path = dir.join("server.crt");
        let server_key_path = dir.join("server.key");
        let client_cert_path = dir.join("client.crt");
        let client_key_path = dir.join("client.key");
        let wrong_ca_path = dir.join("wrong_ca.crt");

        fs::write(&ca_path, &self.ca_pem).expect("write ca");
        fs::write(&server_cert_path, &self.server_cert_pem).expect("write server cert");
        fs::write(&server_key_path, &self.server_key_pem).expect("write server key");
        fs::write(&client_cert_path, &self.client_cert_pem).expect("write client cert");
        fs::write(&client_key_path, &self.client_key_pem).expect("write client key");
        fs::write(&wrong_ca_path, &self.wrong_ca_pem).expect("write wrong ca");

        TestCertPaths {
            ca_path,
            server_cert_path,
            server_key_path,
            _client_cert_path: client_cert_path,
            _client_key_path: client_key_path,
            _wrong_ca_path: wrong_ca_path,
        }
    }
}

struct TestCertPaths {
    ca_path: std::path::PathBuf,
    server_cert_path: std::path::PathBuf,
    server_key_path: std::path::PathBuf,
    _client_cert_path: std::path::PathBuf,
    _client_key_path: std::path::PathBuf,
    _wrong_ca_path: std::path::PathBuf,
}

/// Generate a new Certificate Authority (CA)
fn new_ca(cn: &str) -> (Certificate, Issuer<'static, KeyPair>) {
    let mut params = CertificateParams::new(Vec::default()).expect("empty SAN");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, cn);
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    params.key_usages.push(KeyUsagePurpose::CrlSign);
    let key_pair = KeyPair::generate().expect("ca key");
    let ca = params.self_signed(&key_pair).expect("ca cert");
    let issuer = Issuer::new(params, key_pair);
    (ca, issuer)
}

/// Generate a leaf certificate (server or client) signed by a CA
fn new_leaf(
    cn: &str,
    san: &str,
    eku: ExtendedKeyUsagePurpose,
    issuer: &Issuer<'_, KeyPair>,
) -> (String, String) {
    let mut params = CertificateParams::new(vec![san.to_string()]).expect("SAN");
    params.distinguished_name.push(DnType::CommonName, cn);
    params.use_authority_key_identifier_extension = true;
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params.extended_key_usages.push(eku);
    let key_pair = KeyPair::generate().expect("leaf key");
    let cert = params.signed_by(&key_pair, issuer).expect("leaf cert");
    (cert.pem(), key_pair.serialize_pem())
}

// ============================================================================
// Test Case Definitions (Table-Driven)
// ============================================================================

/// Expected outcome of a TLS connection test
#[derive(Debug, Clone, Copy, PartialEq)]
enum ExpectedResult {
    /// Connection should succeed
    Success,
    /// Connection should fail (TLS handshake or first request)
    Failure,
}

/// Configuration for a single TLS test case
struct TlsTestCase {
    name: &'static str,
    /// Server requires client certificate (mTLS)
    require_client_cert: bool,
    /// Client TLS configuration
    client_tls: ClientTlsSetup,
    /// Expected outcome
    expected: ExpectedResult,
}

/// Client TLS setup options
#[derive(Clone)]
struct ClientTlsSetup {
    /// Trust the server's CA
    trust_server_ca: bool,
    /// Use wrong CA for server verification
    use_wrong_ca: bool,
    /// Send client certificate
    send_client_cert: bool,
    /// Use wrong client certificate (signed by different CA)
    use_wrong_client_cert: bool,
    /// Server name for SNI (None = "localhost")
    server_name: Option<&'static str>,
}

impl Default for ClientTlsSetup {
    fn default() -> Self {
        Self {
            trust_server_ca: true,
            use_wrong_ca: false,
            send_client_cert: false,
            use_wrong_client_cert: false,
            server_name: None,
        }
    }
}

/// Build the table of TLS test cases
fn tls_test_cases() -> Vec<TlsTestCase> {
    vec![
        // ====================================================================
        // Basic TLS Tests
        // ====================================================================
        TlsTestCase {
            name: "TLS_ServerOnly_ClientTrustsCA",
            require_client_cert: false,
            client_tls: ClientTlsSetup {
                trust_server_ca: true,
                ..Default::default()
            },
            expected: ExpectedResult::Success,
        },
        TlsTestCase {
            name: "TLS_ServerOnly_ClientUsesWrongCA",
            require_client_cert: false,
            client_tls: ClientTlsSetup {
                trust_server_ca: false,
                use_wrong_ca: true,
                ..Default::default()
            },
            expected: ExpectedResult::Failure,
        },
        // ====================================================================
        // mTLS Tests
        // ====================================================================
        TlsTestCase {
            name: "mTLS_ValidClientCert",
            require_client_cert: true,
            client_tls: ClientTlsSetup {
                trust_server_ca: true,
                send_client_cert: true,
                ..Default::default()
            },
            expected: ExpectedResult::Success,
        },
        TlsTestCase {
            name: "mTLS_NoClientCert",
            require_client_cert: true,
            client_tls: ClientTlsSetup {
                trust_server_ca: true,
                send_client_cert: false,
                ..Default::default()
            },
            expected: ExpectedResult::Failure,
        },
        TlsTestCase {
            name: "mTLS_WrongClientCA",
            require_client_cert: true,
            client_tls: ClientTlsSetup {
                trust_server_ca: true,
                send_client_cert: true,
                use_wrong_client_cert: true,
                ..Default::default()
            },
            expected: ExpectedResult::Failure,
        },
        // ====================================================================
        // Hostname/SNI Mismatch Tests
        // ====================================================================
        TlsTestCase {
            name: "TLS_HostnameMismatch",
            require_client_cert: false,
            client_tls: ClientTlsSetup {
                trust_server_ca: true,
                server_name: Some("wrong.hostname.com"),
                ..Default::default()
            },
            expected: ExpectedResult::Failure,
        },
    ]
}

// ============================================================================
// OTLP/gRPC TLS Tests (Table-Driven)
// ============================================================================

#[tokio::test]
async fn otlp_grpc_tls_test_matrix() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let certs = TestCerts::generate();
    let temp_dir = TempDir::new().expect("temp dir");
    let paths = certs.write_to_dir(temp_dir.path());

    for test_case in tls_test_cases() {
        println!("Running OTLP/gRPC test: {}", test_case.name);

        let port = portpicker::pick_unused_port().expect("free port");
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

        // Build server TLS config
        let server_tls_config = TlsServerConfig {
            config: TlsConfig {
                cert_file: Some(paths.server_cert_path.clone()),
                key_file: Some(paths.server_key_path.clone()),
                cert_pem: None,
                key_pem: None,
                reload_interval: None,
            },
            client_ca_file: if test_case.require_client_cert {
                Some(paths.ca_path.clone())
            } else {
                None
            },
            client_ca_pem: None,
            include_system_ca_certs_pool: None,
            watch_client_ca: false,
            handshake_timeout: None,
        };

        // Start server
        let server_handle = tokio::spawn(start_tls_otlp_server(addr, server_tls_config));
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Build client TLS config
        let ca_pem = if test_case.client_tls.use_wrong_ca {
            certs.wrong_ca_pem.clone()
        } else if test_case.client_tls.trust_server_ca {
            certs.ca_pem.clone()
        } else {
            String::new()
        };

        let (client_cert, client_key) = if test_case.client_tls.send_client_cert {
            if test_case.client_tls.use_wrong_client_cert {
                (
                    Some(certs.wrong_client_cert_pem.clone()),
                    Some(certs.wrong_client_key_pem.clone()),
                )
            } else {
                (
                    Some(certs.client_cert_pem.clone()),
                    Some(certs.client_key_pem.clone()),
                )
            }
        } else {
            (None, None)
        };

        let client_tls_config = TlsClientConfig {
            config: TlsConfig {
                cert_file: None,
                key_file: None,
                cert_pem: client_cert,
                key_pem: client_key,
                reload_interval: None,
            },
            ca_file: None,
            ca_pem: if ca_pem.is_empty() { None } else { Some(ca_pem) },
            include_system_ca_certs_pool: Some(false),
            server_name: Some(
                test_case
                    .client_tls
                    .server_name
                    .unwrap_or("localhost")
                    .to_string(),
            ),
            ..TlsClientConfig::default()
        };

        let client_settings = GrpcClientSettings {
            grpc_endpoint: format!("https://localhost:{}", port),
            tls: Some(client_tls_config),
            connect_timeout: Duration::from_secs(2),
            ..GrpcClientSettings::default()
        };

        // Try to connect and send request
        let result = try_grpc_request(&client_settings).await;

        match test_case.expected {
            ExpectedResult::Success => {
                assert!(
                    result.is_ok(),
                    "Test '{}' expected success but got error: {:?}",
                    test_case.name,
                    result.err()
                );
            }
            ExpectedResult::Failure => {
                assert!(
                    result.is_err(),
                    "Test '{}' expected failure but succeeded",
                    test_case.name
                );
            }
        }

        server_handle.abort();
    }
}

/// Try to connect and send a gRPC request, returning Ok if successful
async fn try_grpc_request(settings: &GrpcClientSettings) -> Result<(), String> {
    let endpoint = settings
        .build_endpoint_with_tls()
        .await
        .map_err(|e| format!("Failed to build endpoint: {}", e))?;

    let channel = match endpoint.connect().await {
        Ok(ch) => ch,
        Err(e) => return Err(format!("Failed to connect: {}", e)),
    };

    let mut client = LogsServiceClient::new(channel);
    let req = ExportLogsServiceRequest {
        resource_logs: Vec::new(),
    };
    let mut buf = Vec::new();
    req.encode(&mut buf).expect("encode request");

    match client.export(Bytes::from(buf)).await {
        Ok(_) => Ok(()),
        Err(e) => {
            // Connection-level failures indicate TLS issues
            if e.code() == tonic::Code::Unavailable
                || e.code() == tonic::Code::Unknown
                || e.code() == tonic::Code::Internal
            {
                Err(format!("Request failed (likely TLS): {}", e))
            } else {
                // Other errors (like unimplemented) mean connection worked
                Ok(())
            }
        }
    }
}

// ============================================================================
// OTLP/HTTP TLS Tests (Table-Driven)
// ============================================================================

#[tokio::test]
async fn otlp_http_tls_test_matrix() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let certs = TestCerts::generate();
    let temp_dir = TempDir::new().expect("temp dir");
    let paths = certs.write_to_dir(temp_dir.path());

    // Test cases for HTTP (subset of gRPC tests that apply)
    let http_test_cases = vec![
        ("HTTP_TLS_Success", true, false, ExpectedResult::Success),
        ("HTTP_TLS_WrongCA", false, true, ExpectedResult::Failure),
    ];

    for (name, trust_ca, use_wrong_ca, expected) in http_test_cases {
        println!("Running OTLP/HTTP test: {}", name);

        let port = portpicker::pick_unused_port().expect("free port");
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

        let http_tls_config = TlsServerConfig {
            config: TlsConfig {
                cert_file: Some(paths.server_cert_path.clone()),
                key_file: Some(paths.server_key_path.clone()),
                cert_pem: None,
                key_pem: None,
                reload_interval: None,
            },
            client_ca_file: None,
            client_ca_pem: None,
            include_system_ca_certs_pool: None,
            watch_client_ca: false,
            handshake_timeout: None,
        };

        let http_settings = HttpServerSettings {
            listening_addr: addr,
            tls: Some(http_tls_config),
            ..Default::default()
        };

        let server_handle = tokio::spawn(start_tls_otlp_http_server(http_settings));
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Build client
        let ca_pem = if use_wrong_ca {
            &certs.wrong_ca_pem
        } else if trust_ca {
            &certs.ca_pem
        } else {
            ""
        };

        let result = try_https_connection(addr, ca_pem).await;

        match expected {
            ExpectedResult::Success => {
                assert!(
                    result.is_ok(),
                    "HTTP test '{}' expected success but got: {:?}",
                    name,
                    result.err()
                );
            }
            ExpectedResult::Failure => {
                assert!(
                    result.is_err(),
                    "HTTP test '{}' expected failure but succeeded",
                    name
                );
            }
        }

        server_handle.abort();
    }
}

async fn try_https_connection(addr: SocketAddr, ca_pem: &str) -> Result<(), String> {
    use rustls_pki_types::pem::PemObject;

    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_pki_types::CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
        root_store
            .add(cert.map_err(|e| format!("parse cert: {}", e))?)
            .map_err(|e| format!("add cert: {}", e))?;
    }

    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(client_config));

    let tcp_stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| format!("TCP connect: {}", e))?;

    let server_name =
        rustls::pki_types::ServerName::try_from("localhost").map_err(|e| format!("SNI: {}", e))?;

    let _tls_stream = connector
        .connect(server_name, tcp_stream)
        .await
        .map_err(|e| format!("TLS connect: {}", e))?;

    Ok(())
}

// ============================================================================
// OTAP (Arrow) TLS Tests (Table-Driven)
// ============================================================================

#[tokio::test]
async fn otap_tls_test_matrix() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let certs = TestCerts::generate();
    let temp_dir = TempDir::new().expect("temp dir");
    let paths = certs.write_to_dir(temp_dir.path());

    // Test cases for OTAP (subset that applies to Arrow protocol)
    let otap_test_cases = vec![
        (
            "OTAP_TLS_Success",
            false, // require client cert
            true,  // send client cert (irrelevant when not required)
            true,  // trust server CA
            ExpectedResult::Success,
        ),
        (
            "OTAP_mTLS_Success",
            true,  // require client cert
            true,  // send client cert
            true,  // trust server CA
            ExpectedResult::Success,
        ),
        (
            "OTAP_mTLS_NoClientCert",
            true,  // require client cert
            false, // don't send client cert
            true,  // trust server CA
            ExpectedResult::Failure,
        ),
    ];

    for (name, require_client_cert, send_client_cert, trust_ca, expected) in otap_test_cases {
        println!("Running OTAP test: {}", name);

        let port = portpicker::pick_unused_port().expect("free port");
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

        let tls_config = TlsServerConfig {
            config: TlsConfig {
                cert_file: Some(paths.server_cert_path.clone()),
                key_file: Some(paths.server_key_path.clone()),
                cert_pem: None,
                key_pem: None,
                reload_interval: None,
            },
            client_ca_file: if require_client_cert {
                Some(paths.ca_path.clone())
            } else {
                None
            },
            client_ca_pem: None,
            include_system_ca_certs_pool: None,
            watch_client_ca: false,
            handshake_timeout: None,
        };

        let server_handle = tokio::spawn(start_tls_otap_server(addr, tls_config));
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (client_cert, client_key) = if send_client_cert {
            (
                Some(certs.client_cert_pem.clone()),
                Some(certs.client_key_pem.clone()),
            )
        } else {
            (None, None)
        };

        let client_settings = GrpcClientSettings {
            grpc_endpoint: format!("https://localhost:{}", port),
            tls: Some(TlsClientConfig {
                config: TlsConfig {
                    cert_file: None,
                    key_file: None,
                    cert_pem: client_cert,
                    key_pem: client_key,
                    reload_interval: None,
                },
                ca_file: None,
                ca_pem: if trust_ca {
                    Some(certs.ca_pem.clone())
                } else {
                    None
                },
                include_system_ca_certs_pool: Some(false),
                server_name: Some("localhost".to_string()),
                ..TlsClientConfig::default()
            }),
            connect_timeout: Duration::from_secs(2),
            ..GrpcClientSettings::default()
        };

        let result = try_otap_request(&client_settings).await;

        match expected {
            ExpectedResult::Success => {
                assert!(
                    result.is_ok(),
                    "OTAP test '{}' expected success but got: {:?}",
                    name,
                    result.err()
                );
            }
            ExpectedResult::Failure => {
                assert!(
                    result.is_err(),
                    "OTAP test '{}' expected failure but succeeded",
                    name
                );
            }
        }

        server_handle.abort();
    }
}

async fn try_otap_request(settings: &GrpcClientSettings) -> Result<(), String> {
    let endpoint = settings
        .build_endpoint_with_tls()
        .await
        .map_err(|e| format!("Failed to build endpoint: {}", e))?;

    let channel = match endpoint.connect().await {
        Ok(ch) => ch,
        Err(e) => return Err(format!("Failed to connect: {}", e)),
    };

    use otap_df_pdata::proto::opentelemetry::arrow::v1::arrow_logs_service_client::ArrowLogsServiceClient;
    let mut client = ArrowLogsServiceClient::new(channel);

    let request_stream = futures::stream::empty();
    match client.arrow_logs(request_stream).await {
        Ok(_) => Ok(()),
        Err(e) => {
            if e.code() == tonic::Code::Unavailable
                || e.code() == tonic::Code::Unknown
                || e.code() == tonic::Code::Internal
            {
                Err(format!("Request failed (likely TLS): {}", e))
            } else {
                Ok(())
            }
        }
    }
}

// ============================================================================
// Additional Edge Case Tests
// ============================================================================

/// Test that server rejects connection when server certificate file is missing
#[tokio::test]
async fn tls_server_missing_certificate_file() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let temp_dir = TempDir::new().expect("temp dir");
    let certs = TestCerts::generate();
    let paths = certs.write_to_dir(temp_dir.path());

    // Server config with missing cert file (should fail to start)
    let bad_config = TlsServerConfig {
        config: TlsConfig {
            cert_file: Some(std::path::PathBuf::from("/nonexistent/server.crt")),
            key_file: Some(paths.server_key_path.clone()),
            cert_pem: None,
            key_pem: None,
            reload_interval: None,
        },
        client_ca_file: None,
        client_ca_pem: None,
        include_system_ca_certs_pool: None,
        watch_client_ca: false,
        handshake_timeout: None,
    };

    // Attempting to build TLS acceptor should fail
    let result = otap_df_otap::tls_utils::build_tls_acceptor(Some(&bad_config)).await;
    assert!(
        result.is_err(),
        "Should fail with missing certificate file"
    );
}

/// Test that server rejects connection when server key file is missing
#[tokio::test]
async fn tls_server_missing_key_file() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let temp_dir = TempDir::new().expect("temp dir");
    let certs = TestCerts::generate();
    let paths = certs.write_to_dir(temp_dir.path());

    // Server config with missing key file (should fail to start)
    let bad_config = TlsServerConfig {
        config: TlsConfig {
            cert_file: Some(paths.server_cert_path.clone()),
            key_file: Some(std::path::PathBuf::from("/nonexistent/server.key")),
            cert_pem: None,
            key_pem: None,
            reload_interval: None,
        },
        client_ca_file: None,
        client_ca_pem: None,
        include_system_ca_certs_pool: None,
        watch_client_ca: false,
        handshake_timeout: None,
    };

    let result = otap_df_otap::tls_utils::build_tls_acceptor(Some(&bad_config)).await;
    assert!(result.is_err(), "Should fail with missing key file");
}

/// Test that mTLS rejects client with certificate signed by wrong CA
#[tokio::test]
async fn mtls_wrong_client_ca_detailed() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let temp_dir = TempDir::new().expect("temp dir");
    let certs = TestCerts::generate();
    let paths = certs.write_to_dir(temp_dir.path());

    let port = portpicker::pick_unused_port().expect("free port");
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

    // Server requires client cert signed by primary CA
    let tls_config = TlsServerConfig {
        config: TlsConfig {
            cert_file: Some(paths.server_cert_path.clone()),
            key_file: Some(paths.server_key_path.clone()),
            cert_pem: None,
            key_pem: None,
            reload_interval: None,
        },
        client_ca_file: Some(paths.ca_path.clone()),
        client_ca_pem: None,
        include_system_ca_certs_pool: None,
        watch_client_ca: false,
        handshake_timeout: None,
    };

    let server_handle = tokio::spawn(start_tls_otlp_server(addr, tls_config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Client presents cert signed by WRONG CA
    let client_settings = GrpcClientSettings {
        grpc_endpoint: format!("https://localhost:{}", port),
        tls: Some(TlsClientConfig {
            config: TlsConfig {
                cert_file: None,
                key_file: None,
                cert_pem: Some(certs.wrong_client_cert_pem.clone()),
                key_pem: Some(certs.wrong_client_key_pem.clone()),
                reload_interval: None,
            },
            ca_file: None,
            ca_pem: Some(certs.ca_pem.clone()),
            include_system_ca_certs_pool: Some(false),
            server_name: Some("localhost".to_string()),
            ..TlsClientConfig::default()
        }),
        connect_timeout: Duration::from_secs(2),
        ..GrpcClientSettings::default()
    };

    let result = try_grpc_request(&client_settings).await;
    assert!(
        result.is_err(),
        "mTLS should reject client cert signed by wrong CA"
    );

    server_handle.abort();
}

/// Test that insecure_skip_verify is explicitly not supported (security measure)
///
/// Note: This is intentionally not implemented for security reasons.
/// The test verifies that attempting to use it produces a clear error message.
#[tokio::test]
async fn tls_insecure_skip_verify_not_supported() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Client with insecure_skip_verify should fail with a clear error
    let client_settings = GrpcClientSettings {
        grpc_endpoint: "https://localhost:12345".to_string(),
        tls: Some(TlsClientConfig {
            config: TlsConfig::default(),
            ca_file: None,
            ca_pem: None,
            include_system_ca_certs_pool: Some(false),
            server_name: Some("localhost".to_string()),
            insecure_skip_verify: Some(true),
            ..TlsClientConfig::default()
        }),
        connect_timeout: Duration::from_secs(2),
        ..GrpcClientSettings::default()
    };

    let result = try_grpc_request(&client_settings).await;
    // Should fail with clear error about insecure_skip_verify not being supported
    assert!(result.is_err(), "insecure_skip_verify should fail");
    let err = result.unwrap_err();
    assert!(
        err.contains("insecure_skip_verify") && err.contains("not supported"),
        "Error should mention insecure_skip_verify is not supported: {}",
        err
    );
}

// ============================================================================
// Hot-Reload Tests for TLS Certificates
// ============================================================================

/// Test server certificate hot-reload: server rotates its certificate and clients
/// must use the new CA to connect after the reload.
///
/// Scenario:
/// 1. Server starts with cert signed by CA1
/// 2. Client trusting CA1 connects successfully
/// 3. Server cert is rotated to one signed by CA2
/// 4. After reload, client trusting CA2 can connect
/// 5. Client trusting only CA1 fails after the rotation
#[tokio::test]
#[cfg_attr(
    any(target_os = "windows", target_os = "macos"),
    ignore = "Skipping on Windows and macOS due to file watcher flakiness"
)]
async fn tls_server_cert_hot_reload() {
    use otap_df_otap::tls_utils::build_reloadable_server_config;

    let _ = rustls::crypto::ring::default_provider().install_default();

    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path();

    // Generate two CAs and their server certs
    let (ca1, ca1_issuer) = new_ca("CA1");
    let (ca2, ca2_issuer) = new_ca("CA2");

    let (server1_cert, server1_key) =
        new_leaf("localhost", "localhost", ExtendedKeyUsagePurpose::ServerAuth, &ca1_issuer);
    let (server2_cert, server2_key) =
        new_leaf("localhost", "localhost", ExtendedKeyUsagePurpose::ServerAuth, &ca2_issuer);

    // Write CA files
    let ca1_path = path.join("ca1.crt");
    let ca2_path = path.join("ca2.crt");
    fs::write(&ca1_path, ca1.pem()).expect("write ca1");
    fs::write(&ca2_path, ca2.pem()).expect("write ca2");

    // Active cert/key files (will be swapped during reload)
    let active_cert_path = path.join("server.crt");
    let active_key_path = path.join("server.key");
    fs::write(&active_cert_path, &server1_cert).expect("write server1 cert");
    fs::write(&active_key_path, &server1_key).expect("write server1 key");

    // Server config with reload enabled
    let config = TlsServerConfig {
        config: TlsConfig {
            cert_file: Some(active_cert_path.clone()),
            key_file: Some(active_key_path.clone()),
            cert_pem: None,
            key_pem: None,
            reload_interval: Some(Duration::from_secs(1)),
        },
        client_ca_file: None,
        client_ca_pem: None,
        include_system_ca_certs_pool: None,
        watch_client_ca: false,
        handshake_timeout: None,
    };

    // Start server with reloadable config
    let server_config = build_reloadable_server_config(&config)
        .await
        .expect("build reloadable server config");
    let tls_acceptor = std::sync::Arc::new(tokio_rustls::TlsAcceptor::from(server_config));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");

    let acceptor_for_server = tls_acceptor.clone();
    let server_handle = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => continue,
            };
            let acceptor = acceptor_for_server.clone();
            drop(tokio::spawn(async move {
                drop(acceptor.accept(stream).await);
            }));
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Test 1: Client trusting CA1 should succeed
    let result1 = try_tls_connect(addr, &ca1.pem()).await;
    assert!(result1.is_ok(), "Client trusting CA1 should succeed initially");

    // Test 2: Client trusting CA2 should fail (server has CA1 cert)
    let result2 = try_tls_connect(addr, &ca2.pem()).await;
    assert!(result2.is_err(), "Client trusting CA2 should fail before reload");

    // Rotate server cert: write new cert signed by CA2
    tokio::time::sleep(Duration::from_secs(2)).await; // Wait for reload interval

    fs::write(&active_cert_path, &server2_cert).expect("write server2 cert");
    fs::write(&active_key_path, &server2_key).expect("write server2 key");

    // Wait for reload to happen
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Trigger a connection to potentially trigger reload check
    let _ = try_tls_connect(addr, &ca1.pem()).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Test 3: Client trusting CA2 should now succeed
    let result3 = try_tls_connect(addr, &ca2.pem()).await;
    assert!(
        result3.is_ok(),
        "Client trusting CA2 should succeed after reload: {:?}",
        result3.err()
    );

    // Test 4: Client trusting only CA1 should fail after rotation
    let result4 = try_tls_connect(addr, &ca1.pem()).await;
    assert!(
        result4.is_err(),
        "Client trusting CA1 should fail after server cert rotated to CA2"
    );

    server_handle.abort();
}

/// Test client CA hot-reload: server updates the CA it trusts for mTLS clients.
///
/// Note: This test complements the more comprehensive `test_mtls_ca_hot_reload`
/// in `mtls_tests.rs`. It uses the same hot-reload mechanism but with our
/// table-driven test helpers.
///
/// Scenario:
/// 1. Server starts trusting CA1 for client certs
/// 2. Client1 (cert signed by CA1) connects successfully
/// 3. Server's client CA is rotated to CA2
/// 4. After reload, Client2 (cert signed by CA2) can connect
/// 5. Client1 fails after the rotation (no longer trusted)
#[tokio::test]
#[cfg_attr(
    any(target_os = "windows", target_os = "macos"),
    ignore = "Skipping on Windows and macOS due to file watcher flakiness. See mtls_tests.rs for comprehensive hot-reload tests."
)]
async fn tls_client_ca_hot_reload() {
    use otap_df_otap::tls_utils::build_reloadable_server_config;
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    use std::sync::Arc;

    let _ = rustls::crypto::ring::default_provider().install_default();

    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path();

    // Generate server cert (self-signed for simplicity)
    let (server_ca, server_issuer) = new_ca("Server CA");
    let (server_cert, server_key) =
        new_leaf("localhost", "localhost", ExtendedKeyUsagePurpose::ServerAuth, &server_issuer);

    // Generate two client CAs and their client certs
    let (client_ca1, client_ca1_issuer) = new_ca("Client CA1");
    let (client_ca2, client_ca2_issuer) = new_ca("Client CA2");

    let (client1_cert, client1_key) =
        new_leaf("client1", "client1", ExtendedKeyUsagePurpose::ClientAuth, &client_ca1_issuer);
    let (client2_cert, client2_key) =
        new_leaf("client2", "client2", ExtendedKeyUsagePurpose::ClientAuth, &client_ca2_issuer);

    // Write files
    let server_cert_path = path.join("server.crt");
    let server_key_path = path.join("server.key");
    fs::write(&server_cert_path, &server_cert).expect("write server cert");
    fs::write(&server_key_path, &server_key).expect("write server key");

    // Active client CA file (will be swapped during reload)
    let active_client_ca_path = path.join("client_ca.crt");
    fs::write(&active_client_ca_path, client_ca1.pem()).expect("write client ca1");

    // Server config with client CA watching enabled
    let config = TlsServerConfig {
        config: TlsConfig {
            cert_file: Some(server_cert_path.clone()),
            key_file: Some(server_key_path),
            cert_pem: None,
            key_pem: None,
            reload_interval: None,
        },
        client_ca_file: Some(active_client_ca_path.clone()),
        client_ca_pem: None,
        include_system_ca_certs_pool: None,
        watch_client_ca: true, // Enable file watching for hot-reload
        handshake_timeout: None,
    };

    let server_config = build_reloadable_server_config(&config)
        .await
        .expect("build reloadable server config");
    let tls_acceptor = Arc::new(tokio_rustls::TlsAcceptor::from(server_config));

    // Helper to create mTLS client config
    let server_ca_pem = server_ca.pem();
    let create_mtls_client = |client_cert_pem: &str, client_key_pem: &str| {
        let mut root_store = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(server_ca_pem.as_bytes()) {
            root_store.add(cert.expect("parse")).expect("add");
        }

        let client_certs: Vec<_> = CertificateDer::pem_slice_iter(client_cert_pem.as_bytes())
            .map(|c| c.expect("parse"))
            .collect();
        let client_key = PrivateKeyDer::from_pem_slice(client_key_pem.as_bytes()).expect("parse key");

        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_client_auth_cert(client_certs, client_key)
                .expect("build client config"),
        )
    };

    // Test 1: Client1 (signed by CA1) should succeed
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let acceptor_clone = Arc::clone(&tls_acceptor);
        
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            acceptor_clone.accept(stream).await.is_ok()
        });
        
        tokio::time::sleep(Duration::from_millis(10)).await;
        
        let client_config = create_mtls_client(&client1_cert, &client1_key);
        let connector = tokio_rustls::TlsConnector::from(client_config);
        let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let server_name: rustls::pki_types::ServerName<'_> = "localhost".try_into().expect("sni");
        let result = connector.connect(server_name, stream).await;
        
        let server_accepted = server_task.await.expect("server task");
        assert!(result.is_ok() && server_accepted, "Client1 should succeed initially");
    }

    // Test 2: Client2 (signed by CA2) should fail
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let acceptor_clone = Arc::clone(&tls_acceptor);
        
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            acceptor_clone.accept(stream).await.is_ok()
        });
        
        tokio::time::sleep(Duration::from_millis(10)).await;
        
        let client_config = create_mtls_client(&client2_cert, &client2_key);
        let connector = tokio_rustls::TlsConnector::from(client_config);
        let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let server_name: rustls::pki_types::ServerName<'_> = "localhost".try_into().expect("sni");
        let result = connector.connect(server_name, stream).await;
        
        let server_accepted = server_task.await.expect("server task");
        assert!(result.is_err() || !server_accepted, "Client2 should fail before reload");
    }

    // Hot-reload: Switch client CA from CA1 to CA2
    tokio::time::sleep(Duration::from_millis(500)).await;

    let temp_ca_path = path.join("client_ca.tmp");
    fs::write(&temp_ca_path, client_ca2.pem()).expect("write temp ca");
    fs::rename(&temp_ca_path, &active_client_ca_path).expect("atomic rename for hot-reload");

    // Wait for file watcher to detect change
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Test 3: Client2 should now succeed
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let acceptor_clone = Arc::clone(&tls_acceptor);
        
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            acceptor_clone.accept(stream).await.is_ok()
        });
        
        tokio::time::sleep(Duration::from_millis(10)).await;
        
        let client_config = create_mtls_client(&client2_cert, &client2_key);
        let connector = tokio_rustls::TlsConnector::from(client_config);
        let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let server_name: rustls::pki_types::ServerName<'_> = "localhost".try_into().expect("sni");
        let result = connector.connect(server_name, stream).await;
        
        let server_accepted = server_task.await.expect("server task");
        assert!(result.is_ok() && server_accepted, "Client2 should succeed after CA reload");
    }

    // Test 4: Client1 should now fail
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let acceptor_clone = Arc::clone(&tls_acceptor);
        
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            acceptor_clone.accept(stream).await.is_ok()
        });
        
        tokio::time::sleep(Duration::from_millis(10)).await;
        
        let client_config = create_mtls_client(&client1_cert, &client1_key);
        let connector = tokio_rustls::TlsConnector::from(client_config);
        let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let server_name: rustls::pki_types::ServerName<'_> = "localhost".try_into().expect("sni");
        let result = connector.connect(server_name, stream).await;
        
        let server_accepted = server_task.await.expect("server task");
        assert!(result.is_err() || !server_accepted, "Client1 should fail after CA reload");
    }
}

/// Test graceful degradation: server keeps working when CA file becomes corrupted
#[tokio::test]
#[cfg_attr(
    any(target_os = "windows", target_os = "macos"),
    ignore = "Skipping on Windows and macOS due to file watcher flakiness"
)]
async fn tls_client_ca_reload_with_corrupted_file() {
    use otap_df_otap::tls_utils::build_reloadable_server_config;
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};

    let _ = rustls::crypto::ring::default_provider().install_default();

    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path();

    // Generate server cert
    let (server_ca, server_issuer) = new_ca("Server CA");
    let (server_cert, server_key) =
        new_leaf("localhost", "localhost", ExtendedKeyUsagePurpose::ServerAuth, &server_issuer);

    // Generate client CA and cert
    let (client_ca, client_ca_issuer) = new_ca("Client CA");
    let (client_cert, client_key) =
        new_leaf("client", "client", ExtendedKeyUsagePurpose::ClientAuth, &client_ca_issuer);

    // Write files
    let server_cert_path = path.join("server.crt");
    let server_key_path = path.join("server.key");
    fs::write(&server_cert_path, &server_cert).expect("write server cert");
    fs::write(&server_key_path, &server_key).expect("write server key");

    let active_client_ca_path = path.join("client_ca.crt");
    fs::write(&active_client_ca_path, client_ca.pem()).expect("write client ca");

    let config = TlsServerConfig {
        config: TlsConfig {
            cert_file: Some(server_cert_path),
            key_file: Some(server_key_path),
            cert_pem: None,
            key_pem: None,
            reload_interval: None,
        },
        client_ca_file: Some(active_client_ca_path.clone()),
        client_ca_pem: None,
        include_system_ca_certs_pool: None,
        watch_client_ca: true,
        handshake_timeout: None,
    };

    let server_config = build_reloadable_server_config(&config)
        .await
        .expect("build config");
    let tls_acceptor = std::sync::Arc::new(tokio_rustls::TlsAcceptor::from(server_config));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");

    let acceptor_for_server = tls_acceptor.clone();
    let server_handle = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => continue,
            };
            let acceptor = acceptor_for_server.clone();
            drop(tokio::spawn(async move {
                drop(acceptor.accept(stream).await);
            }));
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let server_ca_pem = server_ca.pem();

    // Helper to create mTLS client
    let create_mtls_client = || {
        let mut root_store = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(server_ca_pem.as_bytes()) {
            root_store.add(cert.expect("parse")).expect("add");
        }
        let client_certs: Vec<_> = CertificateDer::pem_slice_iter(client_cert.as_bytes())
            .map(|c| c.expect("parse"))
            .collect();
        let client_key_der = PrivateKeyDer::from_pem_slice(client_key.as_bytes()).expect("parse key");
        std::sync::Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_client_auth_cert(client_certs, client_key_der)
                .expect("build"),
        )
    };

    // Test 1: Client should succeed initially
    let result1 = try_mtls_connect(addr, create_mtls_client()).await;
    assert!(result1.is_ok(), "Client should succeed initially");

    // Corrupt the CA file
    tokio::time::sleep(Duration::from_millis(500)).await;
    fs::write(&active_client_ca_path, "CORRUPTED DATA - NOT A VALID CERT").expect("corrupt file");

    // Wait for file watcher to detect change
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Test 2: Client should STILL succeed (graceful degradation - old CA kept)
    let result2 = try_mtls_connect(addr, create_mtls_client()).await;
    assert!(
        result2.is_ok(),
        "Client should still succeed after corrupted CA reload (graceful degradation): {:?}",
        result2.err()
    );

    server_handle.abort();
}

/// Helper to try a TLS connection with a specific CA
async fn try_tls_connect(addr: SocketAddr, ca_pem: &str) -> Result<(), String> {
    use rustls_pki_types::pem::PemObject;

    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_pki_types::CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
        root_store
            .add(cert.map_err(|e| format!("parse: {}", e))?)
            .map_err(|e| format!("add: {}", e))?;
    }

    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(client_config));
    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect: {}", e))?;

    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .map_err(|e| format!("sni: {}", e))?;

    drop(
        connector
            .connect(server_name, stream)
            .await
            .map_err(|e| format!("tls: {}", e))?,
    );

    Ok(())
}

/// Helper to try an mTLS connection with pre-configured client config
async fn try_mtls_connect(
    addr: SocketAddr,
    client_config: std::sync::Arc<rustls::ClientConfig>,
) -> Result<(), String> {
    let connector = tokio_rustls::TlsConnector::from(client_config);
    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect: {}", e))?;

    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .map_err(|e| format!("sni: {}", e))?;

    drop(
        connector
            .connect(server_name, stream)
            .await
            .map_err(|e| format!("tls: {}", e))?,
    );

    Ok(())
}

// ============================================================================
// Helper Functions to Start Test Servers
// ============================================================================

/// Start a minimal TLS-enabled OTLP gRPC server for testing.
async fn start_tls_otlp_server(addr: SocketAddr, tls_config: TlsServerConfig) {
    use otap_df_otap::tls_utils::build_tls_acceptor;
    use otap_df_pdata::proto::opentelemetry::collector::logs::v1::logs_service_server::{
        LogsService, LogsServiceServer,
    };
    use otap_df_pdata::proto::opentelemetry::collector::logs::v1::{
        ExportLogsServiceRequest as LogsRequest, ExportLogsServiceResponse,
    };
    use tonic::{Request, Response, Status};

    struct NoopLogsService;

    #[tonic::async_trait]
    impl LogsService for NoopLogsService {
        async fn export(
            &self,
            _request: Request<LogsRequest>,
        ) -> Result<Response<ExportLogsServiceResponse>, Status> {
            Ok(Response::new(ExportLogsServiceResponse {
                partial_success: None,
            }))
        }
    }

    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let maybe_tls_acceptor = build_tls_acceptor(Some(&tls_config))
        .await
        .expect("build tls acceptor");

    let server =
        tonic::transport::Server::builder().add_service(LogsServiceServer::new(NoopLogsService));

    if let Some(tls_acceptor) = maybe_tls_acceptor {
        let tls_stream = otap_df_otap::tls_utils::create_tls_stream(
            incoming,
            tls_acceptor,
            tls_config.handshake_timeout,
        );
        let _ = server.serve_with_incoming(tls_stream).await;
    } else {
        let _ = server.serve_with_incoming(incoming).await;
    }
}

/// Start a minimal TLS-enabled OTLP HTTP server for testing.
async fn start_tls_otlp_http_server(settings: HttpServerSettings) {
    use http::{Request, Response, StatusCode};
    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use otap_df_otap::tls_utils::build_tls_acceptor;

    let listener = tokio::net::TcpListener::bind(settings.listening_addr)
        .await
        .expect("bind");

    let tls_acceptor = build_tls_acceptor(settings.tls.as_ref())
        .await
        .expect("build tls acceptor")
        .expect("tls should be configured");

    loop {
        let (tcp_stream, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(_) => continue,
        };

        let acceptor = tls_acceptor.clone();
        drop(tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp_stream).await {
                Ok(s) => s,
                Err(_) => return,
            };

            let service = service_fn(|_req: Request<Incoming>| async move {
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Full::new(Bytes::new()))
                        .expect("build response"),
                )
            });

            drop(hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(tls_stream), service)
                .await);
        }));
    }
}

/// Start a minimal TLS-enabled OTAP (Arrow) gRPC server for testing.
async fn start_tls_otap_server(addr: SocketAddr, tls_config: TlsServerConfig) {
    use otap_df_otap::tls_utils::build_tls_acceptor;
    use otap_df_pdata::proto::opentelemetry::arrow::v1::arrow_logs_service_server::{
        ArrowLogsService, ArrowLogsServiceServer,
    };
    use otap_df_pdata::proto::opentelemetry::arrow::v1::{BatchArrowRecords, BatchStatus};
    use tonic::{Request, Response, Status, Streaming};

    struct NoopArrowLogsService;

    #[tonic::async_trait]
    impl ArrowLogsService for NoopArrowLogsService {
        type ArrowLogsStream =
            std::pin::Pin<Box<dyn futures::Stream<Item = Result<BatchStatus, Status>> + Send>>;

        async fn arrow_logs(
            &self,
            _request: Request<Streaming<BatchArrowRecords>>,
        ) -> Result<Response<Self::ArrowLogsStream>, Status> {
            let stream = futures::stream::empty();
            Ok(Response::new(Box::pin(stream)))
        }
    }

    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let maybe_tls_acceptor = build_tls_acceptor(Some(&tls_config))
        .await
        .expect("build tls acceptor");

    let server = tonic::transport::Server::builder()
        .add_service(ArrowLogsServiceServer::new(NoopArrowLogsService));

    if let Some(tls_acceptor) = maybe_tls_acceptor {
        let tls_stream = otap_df_otap::tls_utils::create_tls_stream(
            incoming,
            tls_acceptor,
            tls_config.handshake_timeout,
        );
        let _ = server.serve_with_incoming(tls_stream).await;
    } else {
        let _ = server.serve_with_incoming(incoming).await;
    }
}
