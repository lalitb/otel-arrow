// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Integration test validating exporter-side TLS/mTLS for the OTLP exporter.

#![cfg(feature = "experimental-tls")]
#![allow(missing_docs)]

mod common;

use common::tls_helpers::{
    assert_logs_export_succeeds, generate_test_certs, start_tonic_tls_server,
};
use otap_df_config::tls::{TlsClientConfig, TlsConfig};
use otap_df_otap::otap_grpc::client_settings::GrpcClientSettings;

#[tokio::test]
async fn otlp_exporter_connects_with_mtls() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let bundle = generate_test_certs();
    let (addr, mut rx, server) = start_tonic_tls_server(
        &bundle.server_cert_pem,
        &bundle.server_key_pem,
        Some(&bundle.ca_pem),
    )
    .await;

    // Build client endpoint with exporter-style TLS config.
    let settings = GrpcClientSettings {
        grpc_endpoint: format!("https://localhost:{}", addr.port()),
        tls: Some(TlsClientConfig {
            config: TlsConfig {
                cert_file: None,
                cert_pem: Some(bundle.client_cert_pem),
                key_file: None,
                key_pem: Some(bundle.client_key_pem),
                reload_interval: None,
            },
            ca_file: None,
            ca_pem: Some(bundle.ca_pem),
            include_system_ca_certs_pool: Some(false),
            server_name: Some("localhost".to_string()),
            ..TlsClientConfig::default()
        }),
        ..GrpcClientSettings::default()
    };

    let endpoint = settings.build_endpoint_with_tls().await.unwrap();
    let channel = endpoint.connect().await.unwrap();
    assert_logs_export_succeeds(channel, &mut rx).await;

    server.abort();
}

#[tokio::test]
async fn otlp_exporter_fails_with_invalid_ca_pem() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let bundle = generate_test_certs();
    let (addr, _rx, server) =
        start_tonic_tls_server(&bundle.server_cert_pem, &bundle.server_key_pem, None).await;

    // Invalid CA PEM should prevent a successful TLS connection.
    let settings = GrpcClientSettings {
        grpc_endpoint: format!("https://localhost:{}", addr.port()),
        tls: Some(TlsClientConfig {
            config: TlsConfig::default(),
            ca_file: None,
            ca_pem: Some("not a pem bundle".to_string()),
            include_system_ca_certs_pool: Some(false),
            server_name: Some("localhost".to_string()),
            ..TlsClientConfig::default()
        }),
        ..GrpcClientSettings::default()
    };

    let endpoint = settings.build_endpoint_with_tls().await.unwrap();
    let connect_res = endpoint.connect().await;
    assert!(connect_res.is_err());

    server.abort();
}

#[tokio::test]
async fn otlp_exporter_allows_http_with_tls_config() {
    let settings = GrpcClientSettings {
        grpc_endpoint: "http://localhost:4317".to_string(),
        tls: Some(TlsClientConfig {
            config: TlsConfig::default(),
            ca_file: None,
            ca_pem: Some("fake pem".to_string()),
            include_system_ca_certs_pool: None,
            server_name: None,
            ..TlsClientConfig::default()
        }),
        ..GrpcClientSettings::default()
    };

    let _ = settings.build_endpoint_with_tls().await.unwrap();
}

#[tokio::test]
async fn otlp_exporter_fails_partial_mtls() {
    let settings = GrpcClientSettings {
        grpc_endpoint: "https://localhost:4317".to_string(),
        tls: Some(TlsClientConfig {
            config: TlsConfig {
                cert_pem: Some("fake cert".to_string()),
                key_pem: None, // Missing key
                ..TlsConfig::default()
            },
            ca_file: None,
            ca_pem: None,
            include_system_ca_certs_pool: None,
            server_name: None,
            ..TlsClientConfig::default()
        }),
        ..GrpcClientSettings::default()
    };

    let result = settings.build_endpoint_with_tls().await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("both client certificate and key must be provided")
    );
}

#[tokio::test]
async fn otlp_exporter_connects_with_tls_only() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let bundle = generate_test_certs();
    let (addr, mut rx, server) =
        start_tonic_tls_server(&bundle.server_cert_pem, &bundle.server_key_pem, None).await;

    // Build client endpoint with TLS but no client identity (TLS-only).
    let settings = GrpcClientSettings {
        grpc_endpoint: format!("https://localhost:{}", addr.port()),
        tls: Some(TlsClientConfig {
            config: TlsConfig::default(), // No client cert/key
            ca_file: None,
            ca_pem: Some(bundle.ca_pem),
            include_system_ca_certs_pool: Some(false),
            server_name: Some("localhost".to_string()),
            ..TlsClientConfig::default()
        }),
        ..GrpcClientSettings::default()
    };

    let endpoint = settings.build_endpoint_with_tls().await.unwrap();
    let channel = endpoint.connect().await.unwrap();
    assert_logs_export_succeeds(channel, &mut rx).await;

    server.abort();
}
