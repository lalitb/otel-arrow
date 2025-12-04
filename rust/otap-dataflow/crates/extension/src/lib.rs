// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Extension system for OTAP dataflow pipeline engine.
//!
//! Extensions are shared components that provide auxiliary functionality to pipeline components.
//! They follow the same pattern as the Go Collector's extension system, providing:
//!
//! - **Authentication extensions**: Provide credentials for outgoing requests or validate incoming requests
//! - **Middleware extensions**: Wrap HTTP/gRPC handlers and clients
//! - **Capability extensions**: Provide additional capabilities like health checks, config watching
//!
//! # Architecture
//!
//! Extensions are registered using the `linkme` crate's distributed slice pattern, similar to
//! how receivers, processors, and exporters are registered in the engine.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      EXTENSIONS                              │
//! │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐       │
//! │  │  Azure Auth  │  │  AWS Auth    │  │  Basic Auth  │  ...  │
//! │  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘       │
//! └─────────┼─────────────────┼─────────────────┼───────────────┘
//!           │                 │                 │
//!           ▼                 ▼                 ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                   ExtensionHost                              │
//! │         (provides extensions to components)                  │
//! └─────────────────────────────────────────────────────────────┘
//!           │
//!           ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │              Pipeline Components                             │
//! │    (Receivers, Processors, Exporters access extensions)      │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use otap_df_extension::{Extension, ExtensionFactory, auth::CredentialProvider};
//!
//! // Define an extension
//! struct MyAuthExtension { /* ... */ }
//!
//! impl Extension for MyAuthExtension {
//!     fn name(&self) -> &'static str { "my-auth" }
//! }
//!
//! impl CredentialProvider for MyAuthExtension {
//!     // ...
//! }
//!
//! // Register via distributed slice
//! #[distributed_slice(EXTENSION_FACTORIES)]
//! static MY_AUTH: ExtensionFactory = ExtensionFactory {
//!     name: "urn:otel:extension:auth:my-auth",
//!     create: |config| { /* ... */ },
//! };
//! ```

pub mod auth;
pub mod error;
pub mod host;
pub mod impls;
pub mod middleware;

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

pub use linkme::distributed_slice;

use error::ExtensionError;
use host::ExtensionHost;

/// Unique identifier for an extension instance.
pub type ExtensionId = std::borrow::Cow<'static, str>;

/// URN identifying the extension type/factory.
pub type ExtensionUrn = std::borrow::Cow<'static, str>;

/// Base trait for all extensions.
///
/// Extensions are shared components that provide auxiliary functionality
/// to pipeline components (receivers, processors, exporters).
///
/// Similar to Go Collector's `extension.Extension` interface.
pub trait Extension: Send + Sync + 'static {
    /// Returns the extension's unique name/identifier.
    fn name(&self) -> &'static str;

    /// Start the extension. Called after all extensions are created.
    ///
    /// Default implementation does nothing.
    fn start(&self, _host: &ExtensionHost) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// Shutdown the extension gracefully.
    ///
    /// Default implementation does nothing.
    fn shutdown(&self) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// Returns this extension as `Any` for downcasting to specific extension traits.
    fn as_any(&self) -> &dyn Any;

    /// Returns this extension as a `ServerAuth` if implemented.
    fn as_server_auth(&self) -> Option<&dyn crate::auth::ServerAuth> {
        None
    }

    /// Returns this extension as a `ClientAuth` if implemented.
    fn as_client_auth(&self) -> Option<&dyn crate::auth::ClientAuth> {
        None
    }

    /// Returns this extension as a `CredentialProvider` if implemented.
    fn as_credential_provider(&self) -> Option<&dyn crate::auth::CredentialProvider> {
        None
    }

    /// Returns this extension as an `HttpServerMiddleware` if implemented.
    fn as_http_server_middleware(&self) -> Option<&dyn crate::middleware::HttpServerMiddleware> {
        None
    }

    /// Returns this extension as an `HttpClientMiddleware` if implemented.
    fn as_http_client_middleware(&self) -> Option<&dyn crate::middleware::HttpClientMiddleware> {
        None
    }

    /// Returns this extension as a `GrpcServerMiddleware` if implemented.
    fn as_grpc_server_middleware(&self) -> Option<&dyn crate::middleware::GrpcServerMiddleware> {
        None
    }

    /// Returns this extension as a `GrpcClientMiddleware` if implemented.
    fn as_grpc_client_middleware(&self) -> Option<&dyn crate::middleware::GrpcClientMiddleware> {
        None
    }
}

/// Factory for creating extension instances.
///
/// Uses the same pattern as `ReceiverFactory`, `ProcessorFactory`, etc.
#[derive(Clone)]
pub struct ExtensionFactory {
    /// URN identifying this extension type (e.g., "urn:otel:extension:auth:azure-cli")
    pub name: &'static str,

    /// Function to create a new extension instance from configuration.
    pub create: fn(config: &serde_json::Value) -> Result<Arc<dyn Extension>, ExtensionError>,
}

impl ExtensionFactory {
    /// Returns the factory name/URN.
    #[inline]
    pub const fn name(&self) -> &'static str {
        self.name
    }
}

/// Distributed slice for extension factory registration.
///
/// Extensions register themselves at compile time using this slice.
///
/// # Example
///
/// ```rust,ignore
/// #[distributed_slice(EXTENSION_FACTORIES)]
/// static MY_EXTENSION: ExtensionFactory = ExtensionFactory {
///     name: "urn:otel:extension:my-ext",
///     create: my_extension_create,
/// };
/// ```
#[distributed_slice]
pub static EXTENSION_FACTORIES: [ExtensionFactory];

/// Global extension factory map, initialized lazily.
static EXTENSION_FACTORY_MAP: OnceLock<HashMap<&'static str, ExtensionFactory>> = OnceLock::new();

/// Returns the global map of extension factories.
pub fn get_extension_factory_map() -> &'static HashMap<&'static str, ExtensionFactory> {
    EXTENSION_FACTORY_MAP.get_or_init(|| {
        EXTENSION_FACTORIES
            .iter()
            .map(|f| (f.name, f.clone()))
            .collect()
    })
}

/// Creates an extension instance by URN.
pub fn create_extension(
    urn: &str,
    config: &serde_json::Value,
) -> Result<Arc<dyn Extension>, ExtensionError> {
    let factory = get_extension_factory_map()
        .get(urn)
        .ok_or_else(|| ExtensionError::NotFound {
            urn: urn.to_string(),
        })?;

    (factory.create)(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestExtension;

    impl Extension for TestExtension {
        fn name(&self) -> &'static str {
            "test-extension"
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn test_extension_trait() {
        let ext = TestExtension;
        assert_eq!(ext.name(), "test-extension");
        let host = crate::host::ExtensionHost::new();
        assert!(ext.start(&host).is_ok());
        assert!(ext.shutdown().is_ok());
    }
}
