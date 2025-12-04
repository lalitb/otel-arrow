// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bearer token authentication extension.
//!
//! This is a simple example extension that provides bearer token authentication
//! for outgoing HTTP requests.
//!
//! # Configuration
//!
//! ```yaml
//! extensions:
//!   - id: my-bearer-auth
//!     extension_urn: "urn:otel:extension:auth:bearer-token"
//!     config:
//!       token: "my-secret-token"
//! ```

use crate::auth::ClientAuth;
use crate::error::ExtensionError;
use crate::{Extension, ExtensionFactory, EXTENSION_FACTORIES};
use async_trait::async_trait;
use linkme::distributed_slice;
use serde::Deserialize;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

/// URN for the bearer token extension.
pub const BEARER_TOKEN_EXTENSION_URN: &str = "urn:otel:extension:auth:bearer-token";

/// Configuration for bearer token authentication.
#[derive(Debug, Clone, Deserialize)]
pub struct BearerTokenConfig {
    /// The bearer token to use for authentication.
    pub token: String,

    /// Optional custom header name (default: "Authorization").
    #[serde(default = "default_header_name")]
    pub header_name: String,
}

fn default_header_name() -> String {
    "Authorization".to_string()
}

/// Bearer token authentication extension.
///
/// Provides bearer token authentication for outgoing HTTP requests.
pub struct BearerTokenExtension {
    config: BearerTokenConfig,
}

impl BearerTokenExtension {
    /// Creates a new bearer token extension.
    pub fn new(config: BearerTokenConfig) -> Self {
        Self { config }
    }

    /// Creates from JSON configuration.
    pub fn from_config(config: &serde_json::Value) -> Result<Self, ExtensionError> {
        let config: BearerTokenConfig =
            serde_json::from_value(config.clone()).map_err(ExtensionError::from_json_error)?;
        Ok(Self::new(config))
    }
}

impl Extension for BearerTokenExtension {
    fn name(&self) -> &'static str {
        "bearer-token"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_client_auth(&self) -> Option<&dyn ClientAuth> {
        Some(self)
    }
}

#[async_trait]
impl ClientAuth for BearerTokenExtension {
    async fn get_request_metadata(&self) -> Result<HashMap<String, String>, ExtensionError> {
        let mut headers = HashMap::new();
        headers.insert(
            self.config.header_name.clone(),
            format!("Bearer {}", self.config.token),
        );
        Ok(headers)
    }

    fn requires_transport_security(&self) -> bool {
        // Bearer tokens should always use TLS
        true
    }
}

/// Factory function for creating bearer token extensions.
fn create_bearer_token_extension(
    config: &serde_json::Value,
) -> Result<Arc<dyn Extension>, ExtensionError> {
    Ok(Arc::new(BearerTokenExtension::from_config(config)?))
}

/// Register the bearer token extension factory.
#[distributed_slice(EXTENSION_FACTORIES)]
pub static BEARER_TOKEN_FACTORY: ExtensionFactory = ExtensionFactory {
    name: BEARER_TOKEN_EXTENSION_URN,
    create: create_bearer_token_extension,
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn test_bearer_token_extension() {
        let config = json!({
            "token": "test-token-123"
        });

        let ext = BearerTokenExtension::from_config(&config).unwrap();
        assert_eq!(ext.name(), "bearer-token");

        let headers = ext.get_request_metadata().await.unwrap();
        assert_eq!(
            headers.get("Authorization"),
            Some(&"Bearer test-token-123".to_string())
        );
    }

    #[tokio::test]
    async fn test_custom_header() {
        let config = json!({
            "token": "my-token",
            "header_name": "X-Custom-Auth"
        });

        let ext = BearerTokenExtension::from_config(&config).unwrap();
        let headers = ext.get_request_metadata().await.unwrap();
        assert_eq!(
            headers.get("X-Custom-Auth"),
            Some(&"Bearer my-token".to_string())
        );
    }

    #[test]
    fn test_requires_transport_security() {
        let config = json!({ "token": "test" });
        let ext = BearerTokenExtension::from_config(&config).unwrap();
        assert!(ext.requires_transport_security());
    }
}
