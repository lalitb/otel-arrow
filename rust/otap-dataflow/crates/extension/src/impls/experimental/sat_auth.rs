// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Service Account Token (SAT) authentication extension.
//!
//! This extension validates Kubernetes Service Account tokens using the TokenReview API.
//! It is a direct port of the Go `satauthextension`.

use crate::auth::{AuthInfo, ServerAuth};
use crate::error::ExtensionError;
use crate::{EXTENSION_FACTORIES, Extension, ExtensionFactory};
use async_trait::async_trait;
use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use kube::{Api, Client};
use linkme::distributed_slice;
use serde::Deserialize;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::OnceCell;

/// URN for the SAT extension.
pub const SAT_EXTENSION_URN: &str = "urn:otel:extension:auth:sat";

/// Trait for validating tokens (allows mocking for tests).
#[async_trait]
pub trait TokenReviewer: Send + Sync {
    async fn review(
        &self,
        token: &str,
        audiences: &[String],
    ) -> Result<TokenReview, ExtensionError>;
}

/// Default implementation using Kubernetes API.
struct K8sTokenReviewer {
    client: OnceCell<Client>,
}

impl K8sTokenReviewer {
    fn new(client: Option<Client>) -> Self {
        let cell = OnceCell::new();
        if let Some(c) = client {
            // If a client is provided (e.g. for testing), set it immediately.
            // We can ignore the error because the cell is new.
            let _ = cell.set(c);
        }
        Self { client: cell }
    }
}

#[async_trait]
impl TokenReviewer for K8sTokenReviewer {
    async fn review(
        &self,
        token: &str,
        audiences: &[String],
    ) -> Result<TokenReview, ExtensionError> {
        let client = self
            .client
            .get_or_try_init(|| async {
                Client::try_default()
                    .await
                    .map_err(|e| ExtensionError::InitializationFailed {
                        message: format!("failed to create k8s client: {}", e),
                    })
            })
            .await?;

        let token_reviews: Api<TokenReview> = Api::all(client.clone());

        let tr = TokenReview {
            spec: TokenReviewSpec {
                token: Some(token.to_string()),
                audiences: Some(audiences.to_vec()),
                ..Default::default()
            },
            ..Default::default()
        };

        let pp = kube::api::PostParams::default();
        token_reviews.create(&pp, &tr).await.map_err(|e| {
            ExtensionError::Other(format!("failed to create TokenReview: {}", e).into())
        })
    }
}

/// Configuration for a specific extension resource.
#[derive(Debug, Clone, Deserialize)]
pub struct ExtensionAuthConfig {
    /// The resource ID that this auth config applies to.
    pub extension_rid: String,

    /// The type of extension (for telemetry).
    pub extension_type: String,

    /// The required namespace of the service account.
    pub service_account_namespace: String,

    /// The allowed service account names.
    pub service_account_names: Vec<String>,
}

/// Configuration for the SAT extension.
#[derive(Debug, Clone, Deserialize)]
pub struct SatConfig {
    /// Allow requests without authentication (legacy/migration flag).
    #[serde(default)]
    pub allow_no_auth: bool,

    /// List of auth configurations.
    #[serde(default)]
    pub extension_auth_configs: Vec<ExtensionAuthConfig>,
}

/// Service Account Token authentication extension.
pub struct SatExtension {
    config: SatConfig,
    audience_map: HashMap<String, ExtensionAuthConfig>,
    reviewer: Arc<dyn TokenReviewer>,
}

impl SatExtension {
    /// Creates a new SAT extension with the default K8s reviewer.
    pub fn new(config: SatConfig) -> Self {
        Self::with_reviewer(config, Arc::new(K8sTokenReviewer::new(None)))
    }

    /// Creates a new SAT extension with a specific K8s client (useful for testing).
    pub fn new_with_client(config: SatConfig, client: Client) -> Self {
        Self::with_reviewer(config, Arc::new(K8sTokenReviewer::new(Some(client))))
    }

    /// Creates a new SAT extension with a custom reviewer.
    pub fn with_reviewer(config: SatConfig, reviewer: Arc<dyn TokenReviewer>) -> Self {
        let mut audience_map = HashMap::new();
        for auth_config in &config.extension_auth_configs {
            let audience = format!("arc-diagnostics:{}", auth_config.extension_rid);
            audience_map.insert(audience, auth_config.clone());
        }

        Self {
            config,
            audience_map,
            reviewer,
        }
    }

    /// Creates from JSON configuration.
    pub fn from_config(config: &serde_json::Value) -> Result<Self, ExtensionError> {
        let config: SatConfig =
            serde_json::from_value(config.clone()).map_err(ExtensionError::from_json_error)?;
        Ok(Self::new(config))
    }
}

impl Extension for SatExtension {
    fn name(&self) -> &'static str {
        "sat-auth"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_server_auth(&self) -> Option<&dyn ServerAuth> {
        Some(self)
    }
}

#[async_trait]
impl ServerAuth for SatExtension {
    async fn authenticate(
        &self,
        headers: &HashMap<String, Vec<String>>,
    ) -> Result<AuthInfo, ExtensionError> {
        let mut auth_header = None;

        // Case-insensitive header lookup
        for (k, v) in headers {
            if k.eq_ignore_ascii_case("authorization") {
                if let Some(val) = v.first() {
                    auth_header = Some(val);
                    break;
                }
            }
        }

        let token = match auth_header {
            Some(h) => {
                if h.len() > 7 && h[..7].eq_ignore_ascii_case("Bearer ") {
                    &h[7..]
                } else if self.config.allow_no_auth {
                    return Ok(AuthInfo::new());
                } else {
                    return Err(ExtensionError::Other(
                        "missing or invalid authorization header".into(),
                    ));
                }
            }
            None => {
                if self.config.allow_no_auth {
                    return Ok(AuthInfo::new());
                }
                return Err(ExtensionError::Other(
                    "missing or invalid authorization header".into(),
                ));
            }
        };

        let audiences: Vec<String> = self.audience_map.keys().cloned().collect();
        let result = self.reviewer.review(token, &audiences).await?;

        let status = result
            .status
            .ok_or_else(|| ExtensionError::Other("TokenReview response missing status".into()))?;

        if !status.authenticated.unwrap_or(false) {
            return Err(ExtensionError::Other(
                format!(
                    "authentication failed: {}",
                    status.error.unwrap_or_else(|| "unknown error".into())
                )
                .into(),
            ));
        }

        let user = status
            .user
            .ok_or_else(|| ExtensionError::Other("TokenReview missing user info".into()))?;

        let username = user.username.unwrap_or_default();
        let matched_audiences = status.audiences.unwrap_or_default();

        // RBAC Check
        for aud in matched_audiences {
            if let Some(config) = self.audience_map.get(&aud) {
                // Parse username (system:serviceaccount:namespace:name)
                let parts: Vec<&str> = username.split(':').collect();
                if parts.len() >= 4 && parts[0] == "system" && parts[1] == "serviceaccount" {
                    let namespace = parts[2];
                    let name = parts[3];

                    if namespace == config.service_account_namespace
                        && config.service_account_names.iter().any(|n| n == name)
                    {
                        // Success
                        let mut info = AuthInfo::with_principal(username);
                        info.metadata
                            .insert("extension_rid".to_string(), config.extension_rid.clone());
                        info.groups = user.groups.unwrap_or_default();
                        return Ok(info);
                    }
                }
            }
        }

        Err(ExtensionError::Other("RBAC validation failed".into()))
    }
}

/// Factory function for creating SAT extensions.
fn create_sat_extension(config: &serde_json::Value) -> Result<Arc<dyn Extension>, ExtensionError> {
    Ok(Arc::new(SatExtension::from_config(config)?))
}

/// Register the SAT extension factory.
#[distributed_slice(EXTENSION_FACTORIES)]
pub static SAT_FACTORY: ExtensionFactory = ExtensionFactory {
    name: SAT_EXTENSION_URN,
    create: create_sat_extension,
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct MockTokenReviewer {
        authenticated: bool,
        username: String,
        audiences: Vec<String>,
    }

    #[async_trait]
    impl TokenReviewer for MockTokenReviewer {
        async fn review(
            &self,
            _token: &str,
            _audiences: &[String],
        ) -> Result<TokenReview, ExtensionError> {
            Ok(TokenReview {
                status: Some(TokenReviewStatus {
                    authenticated: Some(self.authenticated),
                    user: Some(UserInfo {
                        username: Some(self.username.clone()),
                        groups: Some(vec!["system:serviceaccounts".into()]),
                        ..Default::default()
                    }),
                    audiences: Some(self.audiences.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn test_sat_auth_success() {
        let config = SatConfig {
            allow_no_auth: false,
            extension_auth_configs: vec![ExtensionAuthConfig {
                extension_rid: "my-resource".into(),
                extension_type: "test".into(),
                service_account_namespace: "default".into(),
                service_account_names: vec!["my-sa".into()],
            }],
        };

        let reviewer = Arc::new(MockTokenReviewer {
            authenticated: true,
            username: "system:serviceaccount:default:my-sa".into(),
            audiences: vec!["arc-diagnostics:my-resource".into()],
        });

        let ext = SatExtension::with_reviewer(config, reviewer);

        let mut headers = HashMap::new();
        headers.insert("Authorization".into(), vec!["Bearer valid-token".into()]);

        let auth_info = ext.authenticate(&headers).await.unwrap();
        assert_eq!(
            auth_info.principal,
            Some("system:serviceaccount:default:my-sa".into())
        );
        assert_eq!(
            auth_info.metadata.get("extension_rid"),
            Some(&"my-resource".to_string())
        );
    }

    #[tokio::test]
    async fn test_sat_auth_rbac_fail() {
        let config = SatConfig {
            allow_no_auth: false,
            extension_auth_configs: vec![ExtensionAuthConfig {
                extension_rid: "my-resource".into(),
                extension_type: "test".into(),
                service_account_namespace: "default".into(),
                service_account_names: vec!["my-sa".into()],
            }],
        };

        let reviewer = Arc::new(MockTokenReviewer {
            authenticated: true,
            username: "system:serviceaccount:default:other-sa".into(),
            audiences: vec!["arc-diagnostics:my-resource".into()],
        });

        let ext = SatExtension::with_reviewer(config, reviewer);

        let mut headers = HashMap::new();
        headers.insert("Authorization".into(), vec!["Bearer valid-token".into()]);

        let err = ext.authenticate(&headers).await.unwrap_err();
        assert!(
            matches!(err, ExtensionError::Other(msg) if msg.to_string().contains("RBAC validation failed"))
        );
    }

    #[tokio::test]
    async fn test_sat_auth_bypass() {
        let config = SatConfig {
            allow_no_auth: true,
            extension_auth_configs: vec![],
        };

        let reviewer = Arc::new(MockTokenReviewer {
            authenticated: false,
            username: "".into(),
            audiences: vec![],
        });

        let ext = SatExtension::with_reviewer(config, reviewer);

        let headers = HashMap::new();
        let auth_info = ext.authenticate(&headers).await.unwrap();
        assert!(auth_info.principal.is_none());
    }
}
