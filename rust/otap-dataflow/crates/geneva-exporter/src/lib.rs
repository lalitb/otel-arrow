//! Shared Geneva exporter configuration and helpers for OTAP dataflow components.

use geneva_uploader::client::{
    AuthMethod as UploaderAuthMethod, GenevaClient, GenevaClientConfig as UploaderClientConfig,
};
use serde::{Deserialize, Serialize};
use std::{num::NonZeroUsize, path::PathBuf, time::Duration};
use thiserror::Error;

/// Authentication strategy for connecting to Geneva ingestion.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// Azure Managed Identity (system-assigned or user-assigned).
    ManagedIdentity,
    /// Certificate-based authentication using a PKCS#12 bundle.
    Certificate,
    /// Azure Workload Identity (federated Kubernetes identity).
    WorkloadIdentity,
}

/// Retry configuration applied to individual encoded batches.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct BatchRetryConfig {
    /// Maximum number of attempts per batch (initial try + retries).
    pub max_retries: u32,
    /// Initial backoff before the first retry.
    #[serde(with = "humantime_serde")]
    pub initial_interval: Duration,
    /// Maximum backoff between retries.
    #[serde(with = "humantime_serde")]
    pub max_interval: Duration,
    /// Exponential backoff multiplier applied after each retry.
    pub multiplier: f64,
    /// Enable or disable retry logic.
    pub enabled: bool,
}

impl Default for BatchRetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_interval: Duration::from_millis(100),
            max_interval: Duration::from_secs(5),
            multiplier: 2.0,
            enabled: true,
        }
    }
}

/// High-level Geneva exporter settings shared between logs and traces exporters.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct GenevaExporterConfig {
    pub endpoint: String,
    pub environment: String,
    pub account: String,
    pub namespace: String,
    pub region: String,
    pub config_major_version: u32,
    pub tenant: String,
    pub role_name: String,
    pub role_instance: String,

    pub auth_method: AuthMethod,

    /// Optional PKCS#12 certificate path (required when `auth_method == Certificate`).
    pub cert_path: Option<PathBuf>,
    /// Optional certificate password (may be empty for unprotected bundles).
    pub cert_password: Option<String>,

    /// Resource URI for managed/workload identity token acquisition.
    pub identity_resource: Option<String>,
    /// Optional client ID for user-assigned managed identity.
    pub managed_identity_client_id: Option<String>,

    /// Maximum number of concurrent uploads performed per request.
    pub max_concurrent_uploads: NonZeroUsize,
    /// Queue size used when buffering encoded batches locally.
    pub upload_queue_size: NonZeroUsize,

    pub batch_retry: BatchRetryConfig,
}

/// Errors returned while constructing Geneva exporter components.
#[derive(Debug, Error)]
pub enum GenevaExporterError {
    #[error("missing required configuration field `{0}`")]
    MissingField(&'static str),
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("geneva client initialization failed: {0}")]
    ClientInit(String),
}

impl Default for GenevaExporterConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            environment: String::new(),
            account: String::new(),
            namespace: String::new(),
            region: String::new(),
            config_major_version: 0,
            tenant: String::new(),
            role_name: String::new(),
            role_instance: String::new(),
            auth_method: AuthMethod::ManagedIdentity,
            cert_path: None,
            cert_password: None,
            identity_resource: None,
            managed_identity_client_id: None,
            max_concurrent_uploads: NonZeroUsize::new(4).expect("non-zero"),
            upload_queue_size: NonZeroUsize::new(256).expect("non-zero"),
            batch_retry: BatchRetryConfig::default(),
        }
    }
}

impl GenevaExporterConfig {
    /// Validate required fields and produce a fully-populated uploader configuration.
    pub fn to_uploader_config(&self) -> Result<UploaderClientConfig, GenevaExporterError> {
        macro_rules! ensure_present {
            ($field:expr, $name:literal) => {
                if $field.is_empty() {
                    return Err(GenevaExporterError::MissingField($name));
                }
            };
        }

        ensure_present!(self.endpoint, "endpoint");
        ensure_present!(self.environment, "environment");
        ensure_present!(self.account, "account");
        ensure_present!(self.namespace, "namespace");
        ensure_present!(self.region, "region");
        ensure_present!(self.tenant, "tenant");
        ensure_present!(self.role_name, "role_name");
        ensure_present!(self.role_instance, "role_instance");

        if self.config_major_version == 0 {
            return Err(GenevaExporterError::InvalidConfig(
                "config_major_version must be > 0".into(),
            ));
        }

        let uploader_auth = match self.auth_method {
            AuthMethod::Certificate => {
                let path = self
                    .cert_path
                    .clone()
                    .ok_or(GenevaExporterError::MissingField("cert_path"))?;
                UploaderAuthMethod::Certificate {
                    path,
                    password: self.cert_password.clone().unwrap_or_default(),
                }
            }
            AuthMethod::ManagedIdentity => {
                let resource = self.identity_resource.clone().ok_or_else(|| {
                    GenevaExporterError::MissingField("identity_resource (Managed Identity)")
                })?;

                let auth = if let Some(client_id) = &self.managed_identity_client_id {
                    UploaderAuthMethod::UserManagedIdentity {
                        client_id: client_id.clone(),
                    }
                } else {
                    UploaderAuthMethod::SystemManagedIdentity
                };

                auth.with_resource(resource)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geneva_uploader::client::AuthMethod as UploaderAuthMethod;

    fn base_config() -> GenevaExporterConfig {
        GenevaExporterConfig {
            endpoint: "https://ingestion.monitor.azure.com/".into(),
            environment: "prod".into(),
            account: "account".into(),
            namespace: "namespace".into(),
            region: "eastus".into(),
            config_major_version: 1,
            tenant: "tenant".into(),
            role_name: "role".into(),
            role_instance: "instance".into(),
            auth_method: AuthMethod::ManagedIdentity,
            identity_resource: Some("https://monitor.azure.com/".into()),
            ..GenevaExporterConfig::default()
        }
    }

    #[test]
    fn managed_identity_requires_resource() {
        let mut cfg = base_config();
        cfg.identity_resource = None;

        let err = cfg
            .to_uploader_config()
            .expect_err("expected missing resource error");
        assert!(matches!(
            err,
            GenevaExporterError::MissingField("identity_resource (Managed Identity)")
        ));
    }

    #[test]
    fn certificate_requires_path() {
        let mut cfg = base_config();
        cfg.auth_method = AuthMethod::Certificate;
        cfg.identity_resource = None; // not required for certificate

        let err = cfg
            .to_uploader_config()
            .expect_err("expected missing certificate path error");
        assert!(matches!(
            err,
            GenevaExporterError::MissingField("cert_path")
        ));
    }

    #[test]
    fn config_major_version_must_be_non_zero() {
        let mut cfg = base_config();
        cfg.config_major_version = 0;

        let err = cfg
            .to_uploader_config()
            .expect_err("expected invalid config error");
        assert!(matches!(
            err,
            GenevaExporterError::InvalidConfig(ref msg)
            if msg.contains("config_major_version must be > 0")
        ));
    }

    #[test]
    fn managed_identity_successfully_maps_to_uploader_config() {
        let cfg = base_config();
        let uploader_cfg = cfg
            .to_uploader_config()
            .expect("managed identity config should be valid");

        assert_eq!(uploader_cfg.endpoint, cfg.endpoint);
        assert_eq!(uploader_cfg.namespace, cfg.namespace);
        assert_eq!(uploader_cfg.msi_resource, cfg.identity_resource);
        assert!(matches!(
            uploader_cfg.auth_method,
            UploaderAuthMethod::SystemManagedIdentity
        ));
    }

    #[test]
    fn workload_identity_requires_resource() {
        let mut cfg = base_config();
        cfg.auth_method = AuthMethod::WorkloadIdentity;
        cfg.identity_resource = None;

        let err = cfg
            .to_uploader_config()
            .expect_err("expected missing workload identity resource");
        assert!(matches!(
            err,
            GenevaExporterError::MissingField("identity_resource (Workload Identity)")
        ));
    }

    #[test]
    fn certificate_auth_builds_uploader_config() {
        let mut cfg = base_config();
        cfg.auth_method = AuthMethod::Certificate;
        cfg.identity_resource = None;
        cfg.cert_path = Some(PathBuf::from("/tmp/certificate.pfx"));
        cfg.cert_password = Some("secret".into());

        let uploader_cfg = cfg
            .to_uploader_config()
            .expect("certificate configuration should be valid");

        assert!(matches!(
            uploader_cfg.auth_method,
            UploaderAuthMethod::Certificate { .. }
        ));
    }
}
                .with_resource(resource)
            }
            AuthMethod::WorkloadIdentity => {
                let resource = self.identity_resource.clone().ok_or_else(|| {
                    GenevaExporterError::MissingField("identity_resource (Workload Identity)")
                })?;
                UploaderAuthMethod::WorkloadIdentity { resource }
            }
        };

        Ok(UploaderClientConfig {
            endpoint: self.endpoint.clone(),
            environment: self.environment.clone(),
            account: self.account.clone(),
            namespace: self.namespace.clone(),
            region: self.region.clone(),
            config_major_version: self.config_major_version,
            auth_method: uploader_auth,
            tenant: self.tenant.clone(),
            role_name: self.role_name.clone(),
            role_instance: self.role_instance.clone(),
            msi_resource: self.identity_resource.clone(),
        })
    }

    /// Build a `GenevaClient` using the supplied configuration.
    pub fn build_client(&self) -> Result<GenevaClient, GenevaExporterError> {
        let cfg = self.to_uploader_config()?;
        GenevaClient::new(cfg).map_err(GenevaExporterError::ClientInit)
    }
}

/// Extension trait to attach managed identity resource to uploader auth methods.
trait ManagedIdentityExt {
    fn with_resource(self, resource: String) -> UploaderAuthMethod;
}

impl ManagedIdentityExt for UploaderAuthMethod {
    fn with_resource(self, resource: String) -> UploaderAuthMethod {
        match self {
            UploaderAuthMethod::SystemManagedIdentity => UploaderAuthMethod::SystemManagedIdentity,
            UploaderAuthMethod::UserManagedIdentity { client_id } => {
                UploaderAuthMethod::UserManagedIdentity { client_id }
            }
            UploaderAuthMethod::UserManagedIdentityByObjectId { object_id } => {
                UploaderAuthMethod::UserManagedIdentityByObjectId { object_id }
            }
            UploaderAuthMethod::UserManagedIdentityByResourceId { resource_id } => {
                UploaderAuthMethod::UserManagedIdentityByResourceId { resource_id }
            }
            other => other,
        }
    }
}
