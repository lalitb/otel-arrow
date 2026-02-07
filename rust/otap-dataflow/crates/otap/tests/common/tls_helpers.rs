// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Shared TLS/mTLS test helpers for integration tests.
//!
//! Provides cert generation (via rcgen), config builders, and server/client
//! scaffolding so that individual test bodies stay short and focused.

// This module is compiled into every test binary that includes `mod common;`.
// Not every test binary uses every helper, so allow unused items.
#![allow(dead_code, unused_imports)]

use bytes::Bytes;
use otap_df_config::tls::{TlsConfig, TlsServerConfig};
use otap_df_otap::otap_grpc::otlp::client::LogsServiceClient;
use otap_df_otap::tls_utils::build_reloadable_server_config;
use otap_df_pdata::proto::opentelemetry::collector::logs::v1::ExportLogsServiceRequest;
use otap_df_pdata::proto::opentelemetry::collector::logs::v1::ExportLogsServiceResponse;
use otap_df_pdata::proto::opentelemetry::collector::logs::v1::logs_service_server::{
    LogsService, LogsServiceServer,
};
use prost::Message;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tonic::transport::{Channel, Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status};

// ── Certificate bundle ──────────────────────────────────────────────────────

/// Full PKI hierarchy for one CA: CA cert + server leaf + client leaf.
pub struct TlsCertBundle {
    pub ca_pem: String,
    pub server_cert_pem: String,
    pub server_key_pem: String,
    pub client_cert_pem: String,
    pub client_key_pem: String,
}

/// Generate a complete PKI: 1 CA → 1 server leaf (ServerAuth, SAN=localhost)
/// + 1 client leaf (ClientAuth).
pub fn generate_test_certs() -> TlsCertBundle {
    let (ca, ca_issuer) = new_ca("Test CA");
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
    TlsCertBundle {
        ca_pem: ca.pem(),
        server_cert_pem,
        server_key_pem,
        client_cert_pem,
        client_key_pem,
    }
}

/// Generate two independent CA hierarchies (for cross-CA rejection tests).
pub fn generate_separate_ca_certs() -> (TlsCertBundle, TlsCertBundle) {
    (generate_ca_bundle("CA-A"), generate_ca_bundle("CA-B"))
}

fn generate_ca_bundle(cn: &str) -> TlsCertBundle {
    let (ca, ca_issuer) = new_ca(cn);
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
    TlsCertBundle {
        ca_pem: ca.pem(),
        server_cert_pem,
        server_key_pem,
        client_cert_pem,
        client_key_pem,
    }
}

// ── Low-level rcgen helpers ─────────────────────────────────────────────────

fn new_ca(cn: &str) -> (rcgen::Certificate, Issuer<'static, KeyPair>) {
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

pub fn new_leaf(
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

/// Expose `new_ca` for tests that need to build custom PKI hierarchies.
pub fn create_ca(cn: &str) -> (rcgen::Certificate, Issuer<'static, KeyPair>) {
    new_ca(cn)
}

// ── Cert file writers ───────────────────────────────────────────────────────

/// Write all PEM files from a bundle into `dir`.
pub fn write_certs_to_dir(bundle: &TlsCertBundle, dir: &Path) {
    std::fs::write(dir.join("ca.crt"), &bundle.ca_pem).expect("write ca");
    std::fs::write(dir.join("server.crt"), &bundle.server_cert_pem).expect("write server cert");
    std::fs::write(dir.join("server.key"), &bundle.server_key_pem).expect("write server key");
    std::fs::write(dir.join("client.crt"), &bundle.client_cert_pem).expect("write client cert");
    std::fs::write(dir.join("client.key"), &bundle.client_key_pem).expect("write client key");
}

// ── Server TLS config builders ──────────────────────────────────────────────

/// mTLS server config using inline PEM.
pub fn make_server_mtls_config_pem(bundle: &TlsCertBundle) -> TlsServerConfig {
    TlsServerConfig {
        config: TlsConfig {
            cert_pem: Some(bundle.server_cert_pem.clone()),
            key_pem: Some(bundle.server_key_pem.clone()),
            ..TlsConfig::default()
        },
        client_ca_pem: Some(bundle.ca_pem.clone()),
        ..TlsServerConfig::default()
    }
}

/// TLS-only server config using file paths (no client auth).
pub fn make_server_tls_config_file(dir: &Path) -> TlsServerConfig {
    TlsServerConfig {
        config: TlsConfig {
            cert_file: Some(dir.join("server.crt")),
            key_file: Some(dir.join("server.key")),
            ..TlsConfig::default()
        },
        ..TlsServerConfig::default()
    }
}

/// mTLS server config using file paths.
pub fn make_server_mtls_config_file(dir: &Path) -> TlsServerConfig {
    TlsServerConfig {
        config: TlsConfig {
            cert_file: Some(dir.join("server.crt")),
            key_file: Some(dir.join("server.key")),
            ..TlsConfig::default()
        },
        client_ca_file: Some(dir.join("ca.crt")),
        ..TlsServerConfig::default()
    }
}

// ── Client channel builders ─────────────────────────────────────────────────

/// Build a tonic `Channel` with TLS (server-auth only, no client cert).
pub async fn make_tls_client_channel(
    addr: SocketAddr,
    ca_pem: &str,
    server_name: &str,
) -> Result<Channel, Box<dyn std::error::Error>> {
    let tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(ca_pem))
        .domain_name(server_name);
    let endpoint = Channel::from_shared(format!("https://{}:{}", server_name, addr.port()))?
        .tls_config(tls)?;
    let channel = endpoint.connect().await?;
    Ok(channel)
}

/// Build a tonic `Channel` with mTLS (client presents cert).
pub async fn make_mtls_client_channel(
    addr: SocketAddr,
    ca_pem: &str,
    cert_pem: &str,
    key_pem: &str,
    server_name: &str,
) -> Result<Channel, Box<dyn std::error::Error>> {
    let identity = Identity::from_pem(cert_pem, key_pem);
    let tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(ca_pem))
        .identity(identity)
        .domain_name(server_name);
    let endpoint = Channel::from_shared(format!("https://{}:{}", server_name, addr.port()))?
        .tls_config(tls)?;
    let channel = endpoint.connect().await?;
    Ok(channel)
}

// ── Mock gRPC LogsService ───────────────────────────────────────────────────

pub struct LogsServiceMock {
    sender: mpsc::Sender<()>,
}

#[tonic::async_trait]
impl LogsService for LogsServiceMock {
    async fn export(
        &self,
        _request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        self.sender
            .send(())
            .await
            .map_err(|_| Status::internal("send failed"))?;
        Ok(Response::new(ExportLogsServiceResponse {
            partial_success: None,
        }))
    }
}

// ── Server starters ─────────────────────────────────────────────────────────

/// Start a tonic gRPC server with TLS via tonic's own `ServerTlsConfig`.
/// This is the pattern used in existing exporter TLS tests and avoids going
/// through `build_reloadable_server_config` (which uses rustls directly).
pub async fn start_tonic_tls_server(
    server_cert_pem: &str,
    server_key_pem: &str,
    client_ca_pem: Option<&str>,
) -> (SocketAddr, mpsc::Receiver<()>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<()>(16);
    let logs_service = LogsServiceServer::new(LogsServiceMock { sender: tx });

    let server_identity = Identity::from_pem(server_cert_pem, server_key_pem);
    let mut tls = ServerTlsConfig::new().identity(server_identity);
    if let Some(ca) = client_ca_pem {
        tls = tls.client_ca_root(tonic::transport::Certificate::from_pem(ca));
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let handle = tokio::spawn(async move {
        Server::builder()
            .tls_config(tls)
            .expect("tls config")
            .add_service(logs_service)
            .serve_with_incoming(incoming)
            .await
            .expect("server");
    });

    // Yield to let the server task start polling the listener.
    // The OS socket is already bound and listening, so TCP connections won't be
    // refused, but this gives the tonic server a chance to set up its state.
    tokio::task::yield_now().await;

    (addr, rx, handle)
}

/// Start a gRPC LogsService server using `build_reloadable_server_config` +
/// `create_tls_stream` — the real receiver TLS code path. This exercises
/// file-based cert loading and the reloadable cert resolver.
pub async fn start_grpc_server_with_reloadable_tls(
    tls_config: TlsServerConfig,
) -> (SocketAddr, mpsc::Receiver<()>, tokio::task::JoinHandle<()>) {
    let handshake_timeout = tls_config.handshake_timeout;
    let server_config = build_reloadable_server_config(&tls_config)
        .await
        .expect("build server TLS config");
    let acceptor = tokio_rustls::TlsAcceptor::from(server_config);

    let (tx, rx) = mpsc::channel::<()>(16);
    let logs_service = LogsServiceServer::new(LogsServiceMock { sender: tx });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let tls_incoming =
        otap_df_otap::tls_utils::create_tls_stream(incoming, acceptor, handshake_timeout);

    let handle = tokio::spawn(async move {
        Server::builder()
            .add_service(logs_service)
            .serve_with_incoming(tls_incoming)
            .await
            .expect("server");
    });

    tokio::task::yield_now().await;

    (addr, rx, handle)
}

/// Start a raw TLS echo-server using `build_reloadable_server_config`.
/// Returns `(addr, JoinHandle)`.  Useful for low-level TLS handshake tests.
pub async fn start_raw_tls_server(
    tls_config: TlsServerConfig,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let server_config = build_reloadable_server_config(&tls_config)
        .await
        .expect("build server TLS config");
    let acceptor = tokio_rustls::TlsAcceptor::from(server_config);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };
            if let Ok(mut tls) = acceptor.accept(stream).await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                if let Ok(n) = tls.read(&mut buf).await {
                    let _ = tls.write_all(&buf[..n]).await;
                }
            }
        }
    });

    (addr, handle)
}

/// Attempt one raw TLS/mTLS connection and capture both server and client outcomes.
///
/// Returns `(server_accepted, client_failed_or_disconnected)`.
pub async fn attempt_raw_tls_connection(
    acceptor: &Arc<tokio_rustls::TlsAcceptor>,
    ca_pem: &str,
    client_cert_pem: Option<&str>,
    client_key_pem: Option<&str>,
) -> (bool, bool) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let server_acceptor = acceptor.clone();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        server_acceptor.accept(stream).await.is_ok()
    });

    let connector = make_rustls_connector(ca_pem, client_cert_pem, client_key_pem);
    let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let domain = rustls::pki_types::ServerName::try_from("localhost").expect("server name");

    let client_failed_or_disconnected = match connector.connect(domain, stream).await {
        Err(_) => true,
        Ok(mut tls_stream) => {
            let write_result = tls_stream.write_all(b"test").await;
            if write_result.is_err() {
                true
            } else {
                let mut buf = [0u8; 1];
                matches!(tls_stream.read(&mut buf).await, Err(_) | Ok(0))
            }
        }
    };

    let server_accepted = server.await.expect("server task");
    (server_accepted, client_failed_or_disconnected)
}

/// Poll an async predicate until it returns true or the timeout elapses.
///
/// Returns true when the predicate succeeds within the timeout, false otherwise.
pub async fn poll_until<F, Fut>(
    mut predicate: F,
    interval: std::time::Duration,
    timeout: std::time::Duration,
) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let start = std::time::Instant::now();
    loop {
        if predicate().await {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}

// ── Export helper ────────────────────────────────────────────────────────────

/// Send a dummy `ExportLogsServiceRequest` over the given channel and assert
/// that the server received it (via the mpsc receiver).
pub async fn assert_logs_export_succeeds(channel: Channel, rx: &mut mpsc::Receiver<()>) {
    let mut client = LogsServiceClient::new(channel);
    let req = ExportLogsServiceRequest {
        resource_logs: Vec::new(),
    };
    let mut buf = Vec::new();
    req.encode(&mut buf).expect("encode");
    let _ = client.export(Bytes::from(buf)).await.expect("export rpc");

    let observed = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("timeout waiting for server");
    assert!(observed.is_some(), "server should have received a request");
}

/// Build a rustls `TlsConnector` for tests that talk at the TCP/TLS level
/// (not gRPC). `ca_pem` is the CA the client trusts.
pub fn make_rustls_connector(
    ca_pem: &str,
    client_cert_pem: Option<&str>,
    client_key_pem: Option<&str>,
) -> tokio_rustls::TlsConnector {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    use std::io;

    let mut root_store = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_reader_iter(&mut io::BufReader::new(ca_pem.as_bytes())) {
        root_store.add(cert.expect("parse CA cert")).expect("add");
    }

    let config = if let (Some(cert), Some(key)) = (client_cert_pem, client_key_pem) {
        let certs: Vec<_> =
            CertificateDer::pem_reader_iter(&mut io::BufReader::new(cert.as_bytes()))
                .collect::<Result<_, _>>()
                .expect("parse client certs");
        let key = PrivateKeyDer::from_pem_reader(&mut io::BufReader::new(key.as_bytes()))
            .expect("parse client key");
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_client_auth_cert(certs, key)
            .expect("client config with cert")
    } else {
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth()
    };

    tokio_rustls::TlsConnector::from(Arc::new(config))
}

/// Try a raw TLS handshake to the given address. Returns `Ok(())` on success.
pub async fn try_tls_handshake(
    addr: SocketAddr,
    connector: &tokio_rustls::TlsConnector,
    server_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let stream = tokio::net::TcpStream::connect(addr).await?;
    let domain = rustls::pki_types::ServerName::try_from(server_name.to_string())?;
    let _tls = connector.connect(domain, stream).await?;
    Ok(())
}
