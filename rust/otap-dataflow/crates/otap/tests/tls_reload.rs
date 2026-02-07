// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

#![allow(missing_docs)]
#![allow(unused_results)]

#[cfg(feature = "experimental-tls")]
mod common;

#[cfg(feature = "experimental-tls")]
mod tests {
    use crate::common::tls_helpers::{generate_separate_ca_certs, poll_until};
    use otap_df_config::tls::{TlsConfig, TlsServerConfig};
    use otap_df_otap::tls_utils::build_reloadable_server_config;
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
    #[cfg_attr(
        target_os = "windows",
        ignore = "Skipping on Windows due to flakiness. See https://github.com/open-telemetry/otel-arrow/issues/1614"
    )]
    async fn test_tls_reload_integration() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path();
        let cert_path = path.join("server.crt");
        let key_path = path.join("server.key");

        // 1. Generate two independent CA/server chains.
        let (bundle_a, bundle_b) = generate_separate_ca_certs();
        // 2. Start Server with CA1-signed server cert.
        fs::write(&cert_path, &bundle_a.server_cert_pem).expect("write cert failed");
        fs::write(&key_path, &bundle_a.server_key_pem).expect("write key failed");

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
            handshake_timeout: None,
            watch_client_ca: false,
        };

        let addr: SocketAddr = "127.0.0.1:0".parse().expect("Invalid address");
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .expect("Bind failed");
        let local_addr = listener.local_addr().expect("Failed to get local addr");

        let server_handle = start_server(config, listener).await;

        // 3. Connect with Client trusting CA1 (Should Succeed)
        let mut root_store1 = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_reader_iter(&mut BufReader::new(bundle_a.ca_pem.as_bytes()))
        {
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

        // 4. Rotate to CA2-signed server cert.
        // Ensure mtime changes are visible on coarse-granularity filesystems.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let cert_tmp = path.join("server.crt.tmp");
        let key_tmp = path.join("server.key.tmp");
        fs::write(&cert_tmp, &bundle_b.server_cert_pem).expect("write cert tmp failed");
        fs::write(&key_tmp, &bundle_b.server_key_pem).expect("write key tmp failed");
        fs::rename(&cert_tmp, &cert_path).expect("rename cert failed");
        fs::rename(&key_tmp, &key_path).expect("rename key failed");

        // 6. Connect with Client trusting CA2 (Should Succeed)
        let mut root_store2 = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_reader_iter(&mut BufReader::new(bundle_b.ca_pem.as_bytes()))
        {
            root_store2.add(cert.unwrap()).unwrap();
        }

        let client_config2 = rustls::ClientConfig::builder()
            .with_root_certificates(root_store2)
            .with_no_client_auth();
        let connector2 = TlsConnector::from(Arc::new(client_config2));
        let ca2_ready = poll_until(
            || async {
                let stream = match TcpStream::connect(local_addr).await {
                    Ok(stream) => stream,
                    Err(_) => return false,
                };
                connector2.connect(domain.clone(), stream).await.is_ok()
            },
            Duration::from_millis(200),
            Duration::from_secs(10),
        )
        .await;
        assert!(
            ca2_ready,
            "Handshake with CA2 failed (reload did not happen)"
        );

        // 7. Verify Client trusting CA1 now fails
        let ca1_rejected = poll_until(
            || async {
                let stream = match TcpStream::connect(local_addr).await {
                    Ok(stream) => stream,
                    Err(_) => return false,
                };
                connector1.connect(domain.clone(), stream).await.is_err()
            },
            Duration::from_millis(200),
            Duration::from_secs(10),
        )
        .await;
        assert!(ca1_rejected, "Handshake with CA1 should fail after reload");

        // Cleanup
        server_handle.abort();
    }
}
