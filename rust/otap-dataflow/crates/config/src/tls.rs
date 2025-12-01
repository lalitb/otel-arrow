use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Common TLS configuration for both client and server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct TlsConfig {
    /// Path to the CA cert. For a client this verifies the server certificate.
    /// For a server this verifies client certificates.
    /// To use system root CAs, set include_system_ca_certs_pool to true.
    pub ca_file: Option<PathBuf>,

    /// In memory PEM encoded cert.
    pub ca_pem: Option<String>,

    /// If true, load system CA certificates pool in addition to the certificates
    /// configured in this struct.
    /// Note: If this is true and no system certificates are found, the server will fail to start.
    /// "No system certificates are found" means one of the following:
    /// 1. The system has no CA certificates installed at all,
    /// 2. The `rustls-native-certs` crate failed to locate them (e.g., due to permissions or platform issues),
    /// 3. An empty list was returned by the crate.
    /// This distinction is important for deployment: ensure your environment provides accessible system CA certificates.
    pub include_system_ca_certs_pool: Option<bool>,

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
    /// ReloadInterval specifies the duration after which the certificate will be reloaded
    /// If not set, it will never be reloaded.
    /// Format: Standard duration string (e.g., "30s", "5m", "1h").
    pub reload_interval: Option<String>,
}

/// TLS configuration specific to client connections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct TlsClientConfig {
    /// Common TLS configuration.
    #[serde(flatten)]
    pub config: TlsConfig,

    /// In gRPC and HTTP when set to true, this is used to disable the client transport security.
    pub insecure: Option<bool>,

    /// InsecureSkipVerify will enable TLS but not verify the certificate.
    pub insecure_skip_verify: Option<bool>,

    /// ServerName requested by client for virtual hosting.
    pub server_name_override: Option<String>,
}

/// TLS configuration specific to server connections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct TlsServerConfig {
    /// Common TLS configuration.
    #[serde(flatten)]
    pub config: TlsConfig,

    /// Path to the TLS cert to use by the server to verify a client certificate.
    pub client_ca_file: Option<PathBuf>,

    /// Path to the Certificate Revocation List (CRL) to use by the server to verify a client certificate.
    pub client_crl_file: Option<PathBuf>,
    // TODO: Implement certificate reloading
    // /// Reload the ClientCAs file when it is modified
    // pub client_ca_file_reload: Option<bool>,
}
