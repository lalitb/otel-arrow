// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Authentication extension traits.
//!
//! This module provides traits for authentication extensions, similar to
//! Go Collector's `extensionauth` package.
//!
//! # Extension Types
//!
//! - [`ServerAuth`]: Authenticates incoming requests (for receivers)
//! - [`ClientAuth`]: Provides credentials for outgoing requests (for exporters)
//! - [`CredentialProvider`]: Generic credential provider for cloud storage, etc.
//!
//! # Example
//!
//! ```rust,ignore
//! use otap_df_extension::auth::{ClientAuth, Credentials};
//!
//! struct BearerTokenAuth {
//!     token: String,
//! }
//!
//! impl ClientAuth for BearerTokenAuth {
//!     fn get_request_metadata(&self) -> Result<HashMap<String, String>, ExtensionError> {
//!         let mut headers = HashMap::new();
//!         headers.insert("Authorization".to_string(), format!("Bearer {}", self.token));
//!         Ok(headers)
//!     }
//! }
//! ```

use crate::error::ExtensionError;
use async_trait::async_trait;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

/// Server-side authentication for incoming requests.
///
/// Similar to Go Collector's `extensionauth.Server`.
///
/// Implementations authenticate incoming requests and can enrich the context
/// with authentication information (e.g., principal, tenant).
#[async_trait]
pub trait ServerAuth: Send + Sync {
    /// Authenticates an incoming request based on the provided headers/metadata.
    ///
    /// # Arguments
    /// * `headers` - Request headers/metadata as key-value pairs (values can have multiple entries)
    ///
    /// # Returns
    /// * `Ok(AuthInfo)` - Authentication successful, returns auth info
    /// * `Err(ExtensionError)` - Authentication failed
    async fn authenticate(
        &self,
        headers: &HashMap<String, Vec<String>>,
    ) -> Result<AuthInfo, ExtensionError>;
}

/// Client-side authentication for outgoing requests.
///
/// Similar to Go Collector's `extensionauth.HTTPClient` and `extensionauth.GRPCClient`.
///
/// Implementations provide credentials/headers for outgoing HTTP or gRPC requests.
#[async_trait]
pub trait ClientAuth: Send + Sync {
    /// Returns headers/metadata to add to outgoing requests.
    ///
    /// Called before each request to get fresh credentials.
    async fn get_request_metadata(&self) -> Result<HashMap<String, String>, ExtensionError>;

    /// Whether the credentials require a secure transport (TLS).
    ///
    /// Default is `true` for security.
    fn requires_transport_security(&self) -> bool {
        true
    }
}

/// Authentication information extracted from a successful authentication.
#[derive(Debug, Clone, Default)]
pub struct AuthInfo {
    /// The authenticated principal (e.g., username, service account).
    pub principal: Option<String>,

    /// Group memberships or roles.
    pub groups: Vec<String>,

    /// Tenant/namespace identifier for multi-tenancy.
    pub tenant: Option<String>,

    /// Additional metadata from authentication.
    pub metadata: HashMap<String, String>,
}

impl AuthInfo {
    /// Creates a new empty `AuthInfo`.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an `AuthInfo` with a principal.
    pub fn with_principal(principal: impl Into<String>) -> Self {
        Self {
            principal: Some(principal.into()),
            ..Default::default()
        }
    }
}

/// Generic credential provider for cloud services (storage, etc.).
///
/// This is similar to `object_store::CredentialProvider` but abstracted
/// for use across different cloud providers.
///
/// # Example Use Cases
/// - Azure Blob Storage credentials
/// - AWS S3 credentials  
/// - GCP Cloud Storage credentials
#[async_trait]
pub trait CredentialProvider: Send + Sync {
    /// Returns the current access token/credentials.
    ///
    /// Implementations should handle token refresh internally.
    async fn get_credential(&self) -> Result<Credential, ExtensionError>;
}

/// A credential for accessing cloud services.
#[derive(Debug, Clone)]
pub struct Credential {
    /// The access token or key.
    pub token: String,

    /// Token type (e.g., "Bearer", "AWS4-HMAC-SHA256").
    pub token_type: String,

    /// When the credential expires (if applicable).
    pub expires_at: Option<std::time::SystemTime>,
}

impl Credential {
    /// Creates a new bearer token credential.
    pub fn bearer(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            token_type: "Bearer".to_string(),
            expires_at: None,
        }
    }

    /// Creates a new credential with expiration.
    pub fn with_expiry(mut self, expires_at: std::time::SystemTime) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// Checks if the credential is expired.
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .map(|exp| exp <= std::time::SystemTime::now())
            .unwrap_or(false)
    }
}

/// Helper type for boxed async credential futures.
pub type CredentialFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Credential, ExtensionError>> + Send + 'a>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_info() {
        let info = AuthInfo::with_principal("user@example.com");
        assert_eq!(info.principal, Some("user@example.com".to_string()));
        assert!(info.groups.is_empty());
    }

    #[test]
    fn test_credential_bearer() {
        let cred = Credential::bearer("my-token");
        assert_eq!(cred.token, "my-token");
        assert_eq!(cred.token_type, "Bearer");
        assert!(!cred.is_expired());
    }

    #[test]
    fn test_credential_expired() {
        let past = std::time::SystemTime::UNIX_EPOCH;
        let cred = Credential::bearer("token").with_expiry(past);
        assert!(cred.is_expired());
    }
}
