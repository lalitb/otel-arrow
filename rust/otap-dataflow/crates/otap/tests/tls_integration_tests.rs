// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Comprehensive TLS/mTLS integration tests for the OTAP dataflow engine.
//!
//! Covers server-auth TLS, mutual TLS, certificate lifecycle (hot-reload),
//! config validation, and edge cases for both OTLP and OTAP gRPC receivers.
//!
//! Test conventions:
//! - Shared TLS harnesses live in `common/tls_helpers.rs`.
//! - Every `*_succeeds` test must verify data flow, not only TLS handshake.
//! - Reload tests should use eventual assertions (`poll_until`) instead of fixed waits.

#![cfg(feature = "experimental-tls")]
#![allow(missing_docs)]
#![allow(unused_results)]

mod common;

use common::tls_helpers::*;
use otap_df_config::tls::{TlsClientConfig, TlsConfig, TlsServerConfig};
use otap_df_otap::otap_grpc::client_settings::GrpcClientSettings;
use otap_df_otap::tls_utils::{build_reloadable_server_config, load_server_tls_config};
use rcgen::ExtendedKeyUsagePurpose;
use std::time::Duration;
use tempfile::TempDir;

// ═══════════════════════════════════════════════════════════════════════════
//  Category 1: TLS Handshake (Server Auth Only)
// ═══════════════════════════════════════════════════════════════════════════

mod tls_handshake {
    use super::*;

    /// Server presents a CA-signed cert (PEM), client trusts that CA → connection succeeds.
    #[tokio::test]
    async fn tls_server_only_pem_succeeds() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let bundle = generate_test_certs();
        let (addr, mut rx, handle) =
            start_tonic_tls_server(&bundle.server_cert_pem, &bundle.server_key_pem, None).await;

        let channel = make_tls_client_channel(addr, &bundle.ca_pem, "localhost")
            .await
            .expect("connect");
        assert_logs_export_succeeds(channel, &mut rx).await;
        handle.abort();
    }

    /// Same as above but with certs loaded from files via build_reloadable_server_config.
    #[tokio::test]
    async fn tls_server_only_file_succeeds() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let bundle = generate_test_certs();
        let dir = TempDir::new().unwrap();
        write_certs_to_dir(&bundle, dir.path());

        let tls_config = make_server_tls_config_file(dir.path());
        let (addr, mut rx, handle) = start_grpc_server_with_reloadable_tls(tls_config).await;

        let channel = make_tls_client_channel(addr, &bundle.ca_pem, "localhost")
            .await
            .expect("connect with file-based TLS");
        assert_logs_export_succeeds(channel, &mut rx).await;
        handle.abort();
    }

    /// Client has a different CA than the one that signed the server cert → connection fails.
    #[tokio::test]
    async fn tls_untrusted_server_rejected() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (bundle_a, bundle_b) = generate_separate_ca_certs();

        // Server uses CA-A certs, client trusts CA-B
        let (addr, _rx, handle) =
            start_tonic_tls_server(&bundle_a.server_cert_pem, &bundle_a.server_key_pem, None).await;

        let result = make_tls_client_channel(addr, &bundle_b.ca_pem, "localhost").await;
        assert!(result.is_err(), "client trusting wrong CA should fail");
        handle.abort();
    }

    /// Server cert SAN doesn't match the hostname the client connects to.
    #[tokio::test]
    async fn tls_hostname_mismatch_rejected() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (ca, ca_issuer) = create_ca("Test CA");
        // Server cert with SAN=other.example.com, NOT localhost
        let (server_cert_pem, server_key_pem) = new_leaf(
            "other.example.com",
            "other.example.com",
            ExtendedKeyUsagePurpose::ServerAuth,
            &ca_issuer,
        );

        let (addr, _rx, handle) =
            start_tonic_tls_server(&server_cert_pem, &server_key_pem, None).await;

        // Client connects as "localhost" but server SAN is "other.example.com"
        let result = make_tls_client_channel(addr, &ca.pem(), "localhost").await;
        assert!(result.is_err(), "hostname mismatch should fail");
        handle.abort();
    }

    /// Server cert has a zero-length validity window → handshake fails.
    #[tokio::test]
    async fn tls_expired_server_cert_rejected() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (ca, ca_issuer) = create_ca("Test CA");

        // Build a server leaf with not_after = not_before (immediately expired)
        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("SAN");
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "localhost");
        params.use_authority_key_identifier_extension = true;
        params
            .key_usages
            .push(rcgen::KeyUsagePurpose::DigitalSignature);
        params
            .extended_key_usages
            .push(ExtendedKeyUsagePurpose::ServerAuth);
        // Set validity to the past
        let past = time::OffsetDateTime::now_utc() - time::Duration::days(2);
        let expired = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        params.not_before = past;
        params.not_after = expired;
        let key_pair = rcgen::KeyPair::generate().expect("key");
        let cert = params.signed_by(&key_pair, &ca_issuer).expect("sign");

        let (addr, _rx, handle) =
            start_tonic_tls_server(&cert.pem(), &key_pair.serialize_pem(), None).await;

        let ca_pem = ca.pem();
        let result = make_tls_client_channel(addr, &ca_pem, "localhost").await;
        assert!(result.is_err(), "expired server cert should fail");
        handle.abort();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Category 2: mTLS (Mutual Authentication)
// ═══════════════════════════════════════════════════════════════════════════

mod mtls {
    use super::*;

    /// Both sides present valid certs signed by the same CA → connection + data flows.
    #[tokio::test]
    async fn mtls_pem_succeeds() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let bundle = generate_test_certs();

        let (addr, mut rx, handle) = start_tonic_tls_server(
            &bundle.server_cert_pem,
            &bundle.server_key_pem,
            Some(&bundle.ca_pem),
        )
        .await;

        let channel = make_mtls_client_channel(
            addr,
            &bundle.ca_pem,
            &bundle.client_cert_pem,
            &bundle.client_key_pem,
            "localhost",
        )
        .await
        .expect("mtls connect");

        assert_logs_export_succeeds(channel, &mut rx).await;
        handle.abort();
    }

    /// Same as above but with file-based certs via build_reloadable_server_config.
    #[tokio::test]
    async fn mtls_file_succeeds() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let bundle = generate_test_certs();
        let dir = TempDir::new().unwrap();
        write_certs_to_dir(&bundle, dir.path());

        let tls_config = make_server_mtls_config_file(dir.path());
        let (addr, mut rx, handle) = start_grpc_server_with_reloadable_tls(tls_config).await;

        let channel = make_mtls_client_channel(
            addr,
            &bundle.ca_pem,
            &bundle.client_cert_pem,
            &bundle.client_key_pem,
            "localhost",
        )
        .await
        .expect("connect with file-based mTLS");
        assert_logs_export_succeeds(channel, &mut rx).await;
        handle.abort();
    }

    /// Server requires client cert, client sends none → rejected.
    #[tokio::test]
    async fn mtls_client_no_cert_rejected() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let bundle = generate_test_certs();

        let tls_config = make_server_mtls_config_pem(&bundle);
        let server_config = build_reloadable_server_config(&tls_config)
            .await
            .expect("build config");
        let tls_acceptor = std::sync::Arc::new(tokio_rustls::TlsAcceptor::from(server_config));
        let (server_accepted, client_failed_or_disconnected) =
            attempt_raw_tls_connection(&tls_acceptor, &bundle.ca_pem, None, None).await;
        assert!(
            !server_accepted,
            "server must reject connection without client certificate"
        );
        assert!(
            client_failed_or_disconnected,
            "client should observe handshake failure or disconnect after server rejection"
        );
    }

    /// Client cert signed by a different CA than the one the server trusts → rejected.
    #[tokio::test]
    async fn mtls_client_wrong_ca_rejected() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (bundle_a, bundle_b) = generate_separate_ca_certs();

        // Server uses CA-A for everything, including client auth
        let tls_config = make_server_mtls_config_pem(&bundle_a);
        let server_config = build_reloadable_server_config(&tls_config)
            .await
            .expect("build config");
        let tls_acceptor = std::sync::Arc::new(tokio_rustls::TlsAcceptor::from(server_config));
        let (server_accepted, client_failed_or_disconnected) = attempt_raw_tls_connection(
            &tls_acceptor,
            &bundle_a.ca_pem,
            Some(&bundle_b.client_cert_pem),
            Some(&bundle_b.client_key_pem),
        )
        .await;
        assert!(
            !server_accepted,
            "server must reject a client certificate from the wrong CA"
        );
        assert!(
            client_failed_or_disconnected,
            "client should observe handshake failure or disconnect after server rejection"
        );
    }

    /// Server config has cert_pem but no key_pem → build_reloadable_server_config errors.
    #[tokio::test]
    async fn mtls_server_missing_key_rejected() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = TlsServerConfig {
            config: TlsConfig {
                cert_pem: Some("fake cert".to_string()),
                key_pem: None,
                ..TlsConfig::default()
            },
            ..TlsServerConfig::default()
        };

        let result = build_reloadable_server_config(&config).await;
        assert!(result.is_err(), "server cert without key should error");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Category 3: Certificate Lifecycle
// ═══════════════════════════════════════════════════════════════════════════

mod cert_lifecycle {
    use super::*;

    /// Client CA hot-reload with polling (watch_client_ca=false).
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "Skipping on Windows due to flakiness. See https://github.com/open-telemetry/otel-arrow/issues/1614"
    )]
    async fn client_ca_hot_reload_polling() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let dir = TempDir::new().unwrap();
        let path = dir.path();

        let (bundle_a, bundle_b) = generate_separate_ca_certs();
        std::fs::write(path.join("server.crt"), &bundle_a.server_cert_pem).unwrap();
        std::fs::write(path.join("server.key"), &bundle_a.server_key_pem).unwrap();
        std::fs::write(path.join("client_ca.crt"), &bundle_a.ca_pem).unwrap();

        let config = TlsServerConfig {
            config: TlsConfig {
                cert_file: Some(path.join("server.crt")),
                key_file: Some(path.join("server.key")),
                reload_interval: Some(Duration::from_secs(1)),
                ..TlsConfig::default()
            },
            client_ca_file: Some(path.join("client_ca.crt")),
            watch_client_ca: false, // Polling mode
            ..TlsServerConfig::default()
        };

        let server_config = build_reloadable_server_config(&config).await.unwrap();
        let acceptor = std::sync::Arc::new(tokio_rustls::TlsAcceptor::from(server_config));
        let mut probe = start_raw_tls_probe_server(acceptor).await;
        let server_ca_pem = bundle_a.ca_pem.clone();

        // 1. CA-A client accepted
        let (server_accepted, _client_failed_or_disconnected) =
            attempt_raw_tls_connection_with_probe(
                &mut probe,
                &server_ca_pem,
                Some(&bundle_a.client_cert_pem),
                Some(&bundle_a.client_key_pem),
            )
            .await;
        assert!(
            server_accepted,
            "server should accept CA-A client initially"
        );

        // 2. CA-B client rejected initially
        let (server_accepted, _client_failed_or_disconnected) =
            attempt_raw_tls_connection_with_probe(
                &mut probe,
                &server_ca_pem,
                Some(&bundle_b.client_cert_pem),
                Some(&bundle_b.client_key_pem),
            )
            .await;
        assert!(
            !server_accepted,
            "server should reject CA-B client initially"
        );

        // 3. Swap client CA to CA-B and wait for polling to pick it up
        // Ensure mtime advances on coarse filesystems so polling detects the change.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let temp_path = path.join("client_ca.tmp");
        std::fs::write(&temp_path, &bundle_b.ca_pem).unwrap();
        std::fs::rename(&temp_path, path.join("client_ca.crt")).unwrap();

        let mut ca_b_reloaded = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            let (server_accepted, _client_failed_or_disconnected) =
                attempt_raw_tls_connection_with_probe(
                    &mut probe,
                    &server_ca_pem,
                    Some(&bundle_b.client_cert_pem),
                    Some(&bundle_b.client_key_pem),
                )
                .await;
            if server_accepted {
                ca_b_reloaded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            ca_b_reloaded,
            "server should accept CA-B client after polling reload"
        );

        let mut ca_a_rejected = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            let (server_accepted, _client_failed_or_disconnected) =
                attempt_raw_tls_connection_with_probe(
                    &mut probe,
                    &server_ca_pem,
                    Some(&bundle_a.client_cert_pem),
                    Some(&bundle_a.client_key_pem),
                )
                .await;
            if !server_accepted {
                ca_a_rejected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            ca_a_rejected,
            "server should reject CA-A client after polling reload"
        );
        probe.abort();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Category 4: Edge Cases & Security
// ═══════════════════════════════════════════════════════════════════════════

mod edge_cases {
    use super::*;

    /// handshake_timeout is enforced: connecting with a stalled client times out.
    #[tokio::test]
    async fn handshake_timeout_enforced() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let bundle = generate_test_certs();
        let dir = TempDir::new().unwrap();
        write_certs_to_dir(&bundle, dir.path());

        let tls_config = make_server_tls_config_file(dir.path());
        let server_config = build_reloadable_server_config(&tls_config).await.unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(server_config);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let listener_stream = tokio_stream::wrappers::TcpListenerStream::new(listener);

        let short_timeout = Some(Duration::from_millis(100));
        let tls_stream =
            otap_df_otap::tls_utils::create_tls_stream(listener_stream, acceptor, short_timeout);
        let mut tls_stream = Box::pin(tls_stream);

        use futures::StreamExt;
        let server_handle = tokio::spawn(async move {
            // The slow client should timeout; valid client should succeed.
            if let Some(Ok(mut stream)) = tls_stream.next().await {
                use tokio::io::AsyncWriteExt;
                let _ = stream.write_all(b"ok").await;
                true
            } else {
                false
            }
        });

        // Stalled client: TCP connect but never send TLS ClientHello
        let _slow_client = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Valid client
        let connector = make_rustls_connector(&bundle.ca_pem, None, None);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let domain = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut stream = connector
            .connect(domain, stream)
            .await
            .expect("valid client should connect after timeout");

        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 2];
        stream.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"ok");
        assert!(server_handle.await.unwrap());
    }

    /// insecure_skip_verify=true → startup error (not supported).
    #[tokio::test]
    async fn insecure_skip_verify_rejected() {
        let settings = GrpcClientSettings {
            grpc_endpoint: "https://localhost:4317".to_string(),
            tls: Some(TlsClientConfig {
                insecure_skip_verify: Some(true),
                ..TlsClientConfig::default()
            }),
            ..GrpcClientSettings::default()
        };

        let result = settings.build_endpoint_with_tls().await;
        assert!(
            result.is_err(),
            "insecure_skip_verify=true should fail fast"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("insecure_skip_verify")
        );
    }

    /// Cert file larger than 4MB → error.
    #[tokio::test]
    async fn oversized_cert_file_rejected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();
        let cert_path = path.join("large.crt");
        let key_path = path.join("large.key");

        let f = std::fs::File::create(&cert_path).unwrap();
        f.set_len(4 * 1024 * 1024 + 1).unwrap();
        std::fs::write(&key_path, b"fake key").unwrap();

        let config = TlsServerConfig {
            config: TlsConfig {
                cert_file: Some(cert_path),
                key_file: Some(key_path),
                ..TlsConfig::default()
            },
            ..TlsServerConfig::default()
        };

        let result = load_server_tls_config(&config).await;
        assert!(
            result.is_err(),
            "oversized certificate file should be rejected"
        );
        assert!(result.unwrap_err().to_string().contains("is too large"));
    }

    /// Empty string cert_pem → error.
    #[tokio::test]
    async fn empty_cert_pem_rejected() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = TlsServerConfig {
            config: TlsConfig {
                cert_pem: Some(String::new()),
                key_pem: Some("fake key".to_string()),
                ..TlsConfig::default()
            },
            ..TlsServerConfig::default()
        };

        let result = build_reloadable_server_config(&config).await;
        assert!(result.is_err(), "empty cert_pem should error");
    }

    /// Multiple simultaneous TLS connections all succeed (not bottlenecked).
    #[tokio::test]
    async fn concurrent_tls_handshakes() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let bundle = generate_test_certs();
        let (addr, _rx, handle) =
            start_tonic_tls_server(&bundle.server_cert_pem, &bundle.server_key_pem, None).await;

        let mut tasks = Vec::new();
        for _ in 0..10 {
            let ca = bundle.ca_pem.clone();
            tasks.push(tokio::spawn(async move {
                make_tls_client_channel(addr, &ca, "localhost")
                    .await
                    .is_ok()
            }));
        }

        let mut success_count = 0;
        for task in tasks {
            if task.await.unwrap() {
                success_count += 1;
            }
        }
        assert!(
            success_count == 10,
            "all 10/10 concurrent connections should succeed on loopback, got {success_count}"
        );
        handle.abort();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Category 5: Config Validation
// ═══════════════════════════════════════════════════════════════════════════

mod config_validation {
    use super::*;

    /// Empty TlsServerConfig → Ok(None) from load_server_tls_config (TLS disabled).
    #[tokio::test]
    async fn server_config_no_tls_returns_none() {
        let config = TlsServerConfig::default();
        let result = load_server_tls_config(&config).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none(), "empty config should return None");
    }

    /// cert_pem set without key_pem → error.
    #[tokio::test]
    async fn server_config_cert_without_key_errors() {
        let config = TlsServerConfig {
            config: TlsConfig {
                cert_pem: Some("fake cert".to_string()),
                key_pem: None,
                ..TlsConfig::default()
            },
            ..TlsServerConfig::default()
        };

        let result = load_server_tls_config(&config).await;
        assert!(
            result.is_err(),
            "server cert without matching key should be rejected"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("both certificate and key must be provided")
        );
    }

    /// key_pem set without cert_pem → error.
    #[tokio::test]
    async fn server_config_key_without_cert_errors() {
        let config = TlsServerConfig {
            config: TlsConfig {
                cert_pem: None,
                key_pem: Some("fake key".to_string()),
                ..TlsConfig::default()
            },
            ..TlsServerConfig::default()
        };

        let result = load_server_tls_config(&config).await;
        assert!(
            result.is_err(),
            "server key without matching cert should be rejected"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("both certificate and key must be provided")
        );
    }
}
