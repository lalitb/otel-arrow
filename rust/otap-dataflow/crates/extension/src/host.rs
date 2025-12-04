// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Extension host for managing and providing extensions to components.
//!
//! The `ExtensionHost` is similar to Go Collector's `component.Host.GetExtensions()`.
//! It manages all extension instances and provides them to pipeline components.

use crate::auth::{ClientAuth, CredentialProvider, ServerAuth};
use crate::error::ExtensionError;
use crate::middleware::{GrpcClientMiddleware, GrpcServerMiddleware, HttpClientMiddleware, HttpServerMiddleware};
use crate::{Extension, ExtensionId, create_extension};
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

/// Configuration for an extension instance.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ExtensionConfig {
    /// Unique identifier for this extension instance.
    pub id: String,

    /// URN of the extension type (factory).
    pub extension_urn: String,

    /// Extension-specific configuration.
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Host that manages extension instances and provides them to components.
///
/// Similar to Go Collector's `component.Host` interface.
///
/// # Example
///
/// ```rust,ignore
/// let host = ExtensionHost::new();
/// host.add_extension("my-auth", my_auth_extension)?;
///
/// // In a component:
/// let auth = host.get_client_auth("my-auth")?;
/// let headers = auth.get_request_metadata()?;
/// ```
pub struct ExtensionHost {
    extensions: HashMap<ExtensionId, Arc<dyn Extension>>,
}

impl Default for ExtensionHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionHost {
    /// Creates a new empty extension host.
    pub fn new() -> Self {
        Self {
            extensions: HashMap::new(),
        }
    }

    /// Creates an extension host from configuration.
    pub fn from_config(configs: &[ExtensionConfig]) -> Result<Self, ExtensionError> {
        let mut host = Self::new();

        for config in configs {
            let extension = create_extension(&config.extension_urn, &config.config)?;
            host.add_extension(config.id.clone(), extension)?;
        }

        Ok(host)
    }

    /// Adds an extension to the host.
    pub fn add_extension(
        &mut self,
        id: impl Into<ExtensionId>,
        extension: Arc<dyn Extension>,
    ) -> Result<(), ExtensionError> {
        let id = id.into();
        self.extensions.insert(id, extension);
        Ok(())
    }

    /// Returns all extensions.
    ///
    /// Similar to Go Collector's `Host.GetExtensions()`.
    pub fn get_extensions(&self) -> &HashMap<ExtensionId, Arc<dyn Extension>> {
        &self.extensions
    }

    /// Gets an extension by ID.
    pub fn get_extension(&self, id: &str) -> Option<&Arc<dyn Extension>> {
        self.extensions.get(id)
    }

    /// Gets an extension and downcasts it to a specific type.
    pub fn get_extension_as<T: 'static>(&self, id: &str) -> Result<&T, ExtensionError> {
        let ext = self
            .extensions
            .get(id)
            .ok_or_else(|| ExtensionError::NotFoundById { id: id.to_string() })?;

        ext.as_any()
            .downcast_ref::<T>()
            .ok_or_else(|| ExtensionError::CapabilityNotSupported {
                id: id.to_string(),
                capability: std::any::type_name::<T>(),
            })
    }

    /// Gets a server authentication extension by ID.
    ///
    /// Similar to Go Collector's `configauth.Config.GetServerAuthenticator()`.
    pub fn get_server_auth(&self, id: &str) -> Result<&dyn ServerAuth, ExtensionError> {
        let ext = self
            .extensions
            .get(id)
            .ok_or_else(|| ExtensionError::NotFoundById { id: id.to_string() })?;

        ext.as_server_auth()
            .ok_or_else(|| ExtensionError::CapabilityNotSupported {
                id: id.to_string(),
                capability: "ServerAuth",
            })
    }

    /// Gets a client authentication extension by ID.
    ///
    /// Similar to Go Collector's `configauth.Config.GetHTTPClientAuthenticator()`.
    pub fn get_client_auth(&self, id: &str) -> Result<&dyn ClientAuth, ExtensionError> {
        let ext = self
            .extensions
            .get(id)
            .ok_or_else(|| ExtensionError::NotFoundById { id: id.to_string() })?;

        ext.as_client_auth()
            .ok_or_else(|| ExtensionError::CapabilityNotSupported {
                id: id.to_string(),
                capability: "ClientAuth",
            })
    }

    /// Gets a credential provider extension by ID.
    pub fn get_credential_provider(&self, id: &str) -> Result<&dyn CredentialProvider, ExtensionError> {
        let ext = self
            .extensions
            .get(id)
            .ok_or_else(|| ExtensionError::NotFoundById { id: id.to_string() })?;

        ext.as_credential_provider()
            .ok_or_else(|| ExtensionError::CapabilityNotSupported {
                id: id.to_string(),
                capability: "CredentialProvider",
            })
    }

    /// Gets an HTTP server middleware extension by ID.
    pub fn get_http_server_middleware(&self, id: &str) -> Result<&dyn HttpServerMiddleware, ExtensionError> {
        let ext = self
            .extensions
            .get(id)
            .ok_or_else(|| ExtensionError::NotFoundById { id: id.to_string() })?;

        ext.as_http_server_middleware()
            .ok_or_else(|| ExtensionError::CapabilityNotSupported {
                id: id.to_string(),
                capability: "HttpServerMiddleware",
            })
    }

    /// Gets an HTTP client middleware extension by ID.
    pub fn get_http_client_middleware(&self, id: &str) -> Result<&dyn HttpClientMiddleware, ExtensionError> {
        let ext = self
            .extensions
            .get(id)
            .ok_or_else(|| ExtensionError::NotFoundById { id: id.to_string() })?;

        ext.as_http_client_middleware()
            .ok_or_else(|| ExtensionError::CapabilityNotSupported {
                id: id.to_string(),
                capability: "HttpClientMiddleware",
            })
    }

    /// Gets a gRPC server middleware extension by ID.
    pub fn get_grpc_server_middleware(&self, id: &str) -> Result<&dyn GrpcServerMiddleware, ExtensionError> {
        let ext = self
            .extensions
            .get(id)
            .ok_or_else(|| ExtensionError::NotFoundById { id: id.to_string() })?;

        ext.as_grpc_server_middleware()
            .ok_or_else(|| ExtensionError::CapabilityNotSupported {
                id: id.to_string(),
                capability: "GrpcServerMiddleware",
            })
    }

    /// Gets a gRPC client middleware extension by ID.
    pub fn get_grpc_client_middleware(&self, id: &str) -> Result<&dyn GrpcClientMiddleware, ExtensionError> {
        let ext = self
            .extensions
            .get(id)
            .ok_or_else(|| ExtensionError::NotFoundById { id: id.to_string() })?;

        ext.as_grpc_client_middleware()
            .ok_or_else(|| ExtensionError::CapabilityNotSupported {
                id: id.to_string(),
                capability: "GrpcClientMiddleware",
            })
    }

    /// Starts all extensions.
    pub fn start_all(&self) -> Result<(), ExtensionError> {
        for (id, ext) in &self.extensions {
            ext.start(self).map_err(|e| ExtensionError::StartFailed {
                message: format!("extension '{}': {}", id, e),
            })?;
        }
        Ok(())
    }

    /// Shuts down all extensions.
    pub fn shutdown_all(&self) -> Result<(), ExtensionError> {
        let mut errors = Vec::new();

        for (id, ext) in &self.extensions {
            if let Err(e) = ext.shutdown() {
                errors.push(format!("extension '{}': {}", id, e));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ExtensionError::ShutdownFailed {
                message: errors.join("; "),
            })
        }
    }
}

/// Reference to an extension by ID, used in component configurations.
///
/// Similar to Go Collector's `configauth.Config` or `configmiddleware.Config`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ExtensionRef {
    /// The ID of the extension to reference.
    pub id: String,
}

impl ExtensionRef {
    /// Creates a new extension reference.
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }

    /// Resolves the extension from the host.
    pub fn resolve<'a>(&self, host: &'a ExtensionHost) -> Result<&'a Arc<dyn Extension>, ExtensionError> {
        host.get_extension(&self.id)
            .ok_or_else(|| ExtensionError::NotFoundById {
                id: self.id.clone(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestExtension {
        name: &'static str,
    }

    impl Extension for TestExtension {
        fn name(&self) -> &'static str {
            self.name
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn test_extension_host() {
        let mut host = ExtensionHost::new();

        let ext = Arc::new(TestExtension { name: "test" });
        host.add_extension("test-ext", ext).unwrap();

        assert!(host.get_extension("test-ext").is_some());
        assert!(host.get_extension("nonexistent").is_none());
    }

    #[test]
    fn test_extension_ref() {
        let mut host = ExtensionHost::new();
        let ext = Arc::new(TestExtension { name: "test" });
        host.add_extension("my-ext", ext).unwrap();

        let ext_ref = ExtensionRef::new("my-ext");
        assert!(ext_ref.resolve(&host).is_ok());

        let bad_ref = ExtensionRef::new("nonexistent");
        assert!(bad_ref.resolve(&host).is_err());
    }

    #[test]
    fn test_start_shutdown() {
        let mut host = ExtensionHost::new();
        let ext = Arc::new(TestExtension { name: "test" });
        host.add_extension("test", ext).unwrap();

        assert!(host.start_all().is_ok());
        assert!(host.shutdown_all().is_ok());
    }
}
