// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Common TLS configuration for both client and server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct TlsConfig {
    /// Path to the TLS cert to use for TLS required connections.
    pub cert_file: Option<PathBuf>,

    /// In memory PEM encoded TLS cert to use for TLS required connections.
    pub cert_pem: Option<String>,

    /// Path to the TLS key to use for TLS required connections.
    pub key_file: Option<PathBuf>,

    /// In memory PEM encoded TLS key to use for TLS required connections.
    pub key_pem: Option<String>,
    // TODO: Implement these fields
    // /// MinVersion sets the minimum TLS version that is acceptable.
    // /// If not set, TLS 1.2 will be used.
    // pub min_version: Option<String>,

    // /// MaxVersion sets the maximum TLS version that is acceptable.
    // pub max_version: Option<String>,

    // /// CipherSuites is a list of TLS cipher suites that the TLS transport can use.
    // pub cipher_suites: Option<Vec<String>>,

    // /// Trusted platform module configuration
    // pub tpm_config: Option<TpmConfig>,
    /// ReloadInterval specifies the duration after which the certificate will be reloaded.
    /// If not set, it defaults to 5 minutes (300 seconds).
    /// Format: Standard duration string (e.g., "30s", "5m", "1h").
    pub reload_interval: Option<String>,
}

/// TLS configuration specific to client connections.
///
/// This configuration is used by components that initiate TLS connections, such as:
/// - **Exporters**: When sending telemetry to a backend.
/// - **Receivers (Scrapers)**: When pulling data from a target (e.g. Prometheus).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct TlsClientConfig {
    /// Common TLS configuration.
    #[serde(flatten)]
    pub config: TlsConfig,

    /// Path to the CA cert. For a client this verifies the server certificate.
    /// To use system root CAs, set include_system_ca_certs_pool to true.
    ///
    /// Used by Exporters and Scrapers.
    pub ca_file: Option<PathBuf>,

    /// In memory PEM encoded cert.
    ///
    /// Used by Exporters and Scrapers.
    pub ca_pem: Option<String>,

    /// Controls whether system CA certificates are loaded for certificate verification.
    ///
    /// # Behavior
    ///
    /// - **`None` or `Some(false)`**: System CA certificates are **not** loaded. Only explicitly
    ///   configured CA sources (`ca_file`, `ca_pem`) are used for verification.
    ///   If no CA is configured, server certificate verification uses only the provided CAs.
    ///
    /// - **`Some(true)`**: System CA certificates are loaded from the OS trust store **in addition to**
    ///   any user-provided CA files (`ca_file`, `ca_pem`). Both sources are combined
    ///   into a single trust store for verification.
    ///
    /// # Failure Conditions
    ///
    /// When set to `true`, the client will fail to start if no system certificates are found.
    /// This can happen when:
    /// 1. The system has no CA certificates installed at all
    /// 2. The `rustls-native-certs` crate failed to locate them (e.g., due to permissions or
    ///    platform-specific issues)
    /// 3. An empty list was returned by the crate
    ///
    /// # Deployment Note
    ///
    /// Ensure your environment provides accessible system CA certificates when enabling this option.
    /// Container environments may need to install CA certificate packages (e.g., `ca-certificates`
    /// on Debian/Ubuntu, `ca-certificates-bundle` on Alpine).
    ///
    /// Used by Exporters and Scrapers.
    pub include_system_ca_certs_pool: Option<bool>,

    /// In gRPC and HTTP when set to true, this is used to disable the client transport security.
    ///
    /// Used by Exporters and Scrapers.
    pub insecure: Option<bool>,

    /// InsecureSkipVerify will enable TLS but not verify the certificate.
    ///
    /// Used by Exporters and Scrapers.
    pub insecure_skip_verify: Option<bool>,

    /// ServerName requested by client for virtual hosting.
    ///
    /// This is required for the TLS SNI (Server Name Indication) handshake when the
    /// connection address (e.g. IP address) does not match the hostname in the
    /// server's certificate.
    ///
    /// Used by Exporters and Scrapers.
    pub server_name_override: Option<String>,
}

/// TLS configuration specific to server connections.
///
/// This configuration is used by components that accept TLS connections, such as:
/// - **Receivers**: When accepting telemetry from agents or other collectors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct TlsServerConfig {
    /// Common TLS configuration.
    #[serde(flatten)]
    pub config: TlsConfig,

    /// Path to the TLS cert to use by the server to verify a client certificate.
    pub client_ca_file: Option<PathBuf>,

    /// In memory PEM encoded cert to use by the server to verify a client certificate.
    pub client_ca_pem: Option<String>,

    /// Controls whether system CA certificates are loaded for client certificate verification.
    ///
    /// # Behavior
    ///
    /// - **`None` or `Some(false)`**: System CA certificates are **not** loaded. Only explicitly
    ///   configured CA sources (`client_ca_file`, `client_ca_pem`) are used for verification.
    ///   If no CA is configured, client certificate verification is disabled.
    ///
    /// - **`Some(true)`**: System CA certificates are loaded from the OS trust store **in addition to**
    ///   any user-provided CA files (`client_ca_file`, `client_ca_pem`). Both sources are combined
    ///   into a single trust store for verification.
    ///
    /// # Failure Conditions
    ///
    /// When set to `true`, the server will fail to start if no system certificates are found.
    /// This can happen when:
    /// 1. The system has no CA certificates installed at all
    /// 2. The `rustls-native-certs` crate failed to locate them (e.g., due to permissions or
    ///    platform-specific issues)
    /// 3. An empty list was returned by the crate
    ///
    /// # Deployment Note
    ///
    /// Ensure your environment provides accessible system CA certificates when enabling this option.
    /// Container environments may need to install CA certificate packages (e.g., `ca-certificates`
    /// on Debian/Ubuntu, `ca-certificates-bundle` on Alpine).
    pub include_system_ca_certs_pool: Option<bool>,

    /// Path to the Certificate Revocation List (CRL) to use by the server to verify a client certificate.
    pub client_crl_file: Option<PathBuf>,
    // Note: Client CA/CRL reloading is currently implemented via polling (using `reload_interval`
    // from TlsConfig). The server periodically checks file modification times and reloads if changed.
    //
    // TODO: Implement file watcher-based reloading for immediate response to certificate changes.
    // This would provide faster reload than polling and be more efficient for infrequent changes.
    // /// Reload the ClientCAs file when it is modified (using file watcher instead of polling)
    // pub client_ca_file_reload: Option<bool>,
}
