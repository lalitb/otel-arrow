// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

#![allow(missing_docs)]
#![allow(unused_results)]

#[cfg(feature = "experimental-tls")]
mod tests {
    use otap_df_config::tls::{TlsConfig, TlsServerConfig};
    use otap_df_otap::tls_utils::build_reloadable_server_config;
    use rcgen::{BasicConstraints, Certificate, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
    use rustls_pki_types::CertificateDer;
    use rustls_pki_types::pem::PemObject;
    use std::fs;
    use std::io::BufReader;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    /// Generated CA certificate with the Certificate object for signing
    struct GeneratedCa {
        cert_pem: String,
        cert: Certificate,
        key_pair: KeyPair,
    }

    /// Generated server certificate
    struct GeneratedServerCert {
        cert_pem: String,
        key_pem: String,
    }

    /// Generate a self-signed CA certificate using rcgen.
    fn generate_ca(cn: &str) -> GeneratedCa {
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, cn);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];

        let key_pair = KeyPair::generate().expect("Failed to generate key pair");
        let cert = params
            .self_signed(&key_pair)
            .expect("Failed to self-sign CA certificate");

        GeneratedCa {
            cert_pem: cert.pem(),
            cert,
            key_pair,
        }
    }

    /// Generate a server certificate signed by a CA using rcgen.
    fn generate_server_cert(cn: &str, ca: &GeneratedCa) -> GeneratedServerCert {
        let mut params = CertificateParams::new(vec!["localhost".to_string()])
            .expect("Failed to create cert params");
        params.distinguished_name.push(DnType::CommonName, cn);
        params.is_ca = IsCa::ExplicitNoCa;

        let key_pair = KeyPair::generate().expect("Failed to generate key pair");

        let cert = params
            .signed_by(&key_pair, &ca.cert, &ca.key_pair)
            .expect("Failed to sign server certificate");

        GeneratedServerCert {
            cert_pem: cert.pem(),
            key_pem: key_pair.serialize_pem(),
        }
    }

    async fn start_server(
        config: TlsServerConfig,
        listener: tokio::net::TcpListener,
    ) -> tokio::task::JoinHandle<()> {
        let server_config = build_reloadable_server_config(&config)
            .await
            .expect("Failed to build server config");
        let acceptor = tokio_rustls::TlsAcceptor::from(server_config);

        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("Accept failed");
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(mut stream) = acceptor.accept(stream).await {
                        let mut buf = [0; 1024];
                        if let Ok(n) = stream.read(&mut buf).await {
                            // Ignore write errors in test server
                            let _ = stream.write_all(&buf[..n]).await;
                        }
                    }
                });
            }
        })
    }

    #[tokio::test]
    async fn test_tls_reload_integration() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path();
        let cert_path = path.join("server.crt");
        let key_path = path.join("server.key");

        // 1. Generate CA1 and Server1 using rcgen
        let ca1 = generate_ca("Test CA 1");
        let server1 = generate_server_cert("localhost", &ca1);

        // 2. Start Server with Server1
        fs::write(&cert_path, &server1.cert_pem).expect("write cert failed");
        fs::write(&key_path, &server1.key_pem).expect("write key failed");

        let config = TlsServerConfig {
            config: TlsConfig {
                cert_file: Some(cert_path.clone()),
                key_file: Some(key_path.clone()),
                reload_interval: Some(Duration::from_secs(1)),
                cert_pem: None,
                key_pem: None,
            },
            client_ca_file: None,
            client_ca_pem: None,
            include_system_ca_certs_pool: None,
            watch_client_ca: false,
            handshake_timeout: None,
        };

        let addr: SocketAddr = "127.0.0.1:0".parse().expect("Invalid address");
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .expect("Bind failed");
        let local_addr = listener.local_addr().expect("Failed to get local addr");

        let server_handle = start_server(config, listener).await;

        // 3. Connect with Client trusting CA1 (Should Succeed)
        let mut root_store1 = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_reader_iter(&mut BufReader::new(ca1.cert_pem.as_bytes())) {
            root_store1.add(cert.unwrap()).unwrap();
        }

        let client_config1 = rustls::ClientConfig::builder()
            .with_root_certificates(root_store1)
            .with_no_client_auth();
        let connector1 = TlsConnector::from(Arc::new(client_config1));

        let stream = TcpStream::connect(local_addr).await.unwrap();
        let domain = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let _stream = connector1
            .connect(domain.clone(), stream)
            .await
            .expect("Handshake with CA1 failed");

        // 4. Rotate to Server2 (signed by CA2)
        tokio::time::sleep(Duration::from_secs(3)).await; // Wait for reload interval

        // Generate CA2 and Server2 now to ensure mtime is different
        let ca2 = generate_ca("Test CA 2");
        let server2 = generate_server_cert("localhost", &ca2);

        fs::write(&cert_path, &server2.cert_pem).expect("write cert failed");
        fs::write(&key_path, &server2.key_pem).expect("write key failed");

        // 5. Trigger Reload by making a connection (async reload happens in background)
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Make a dummy connection to trigger the reload check
        // This connection will use the old cert but spawn async reload
        let stream = TcpStream::connect(local_addr).await.unwrap();
        let _ = connector1.connect(domain.clone(), stream).await; // May succeed or fail, doesn't matter

        // Wait for async reload to complete
        tokio::time::sleep(Duration::from_millis(200)).await;

        // 6. Connect with Client trusting CA2 (Should Succeed)
        let mut root_store2 = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_reader_iter(&mut BufReader::new(ca2.cert_pem.as_bytes())) {
            root_store2.add(cert.unwrap()).unwrap();
        }

        let client_config2 = rustls::ClientConfig::builder()
            .with_root_certificates(root_store2)
            .with_no_client_auth();
        let connector2 = TlsConnector::from(Arc::new(client_config2));

        let stream = TcpStream::connect(local_addr).await.unwrap();
        let _stream = connector2
            .connect(domain.clone(), stream)
            .await
            .expect("Handshake with CA2 failed (Reload didn't happen?)");

        // 7. Verify Client trusting CA1 now fails
        let stream = TcpStream::connect(local_addr).await.unwrap();
        let result = connector1.connect(domain, stream).await;
        assert!(
            result.is_err(),
            "Handshake with CA1 should fail after reload"
        );

        // Cleanup
        server_handle.abort();
    }
}
