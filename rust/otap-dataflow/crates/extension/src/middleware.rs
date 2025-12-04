// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Middleware extension traits.
//!
//! This module provides traits for middleware extensions, similar to
//! Go Collector's `extensionmiddleware` package.
//!
//! Middleware extensions wrap HTTP handlers/clients or gRPC services
//! to add cross-cutting concerns like logging, rate limiting, tracing, etc.

use crate::error::ExtensionError;

/// HTTP server middleware.
///
/// Similar to Go Collector's `extensionmiddleware.HTTPServer`.
///
/// Implementations wrap HTTP handlers to add server-side functionality.
pub trait HttpServerMiddleware: Send + Sync {
    /// Wraps an HTTP handler function.
    ///
    /// The returned function should call the `next` handler after
    /// performing any pre/post processing.
    fn wrap_handler<H, R>(&self, next: H) -> Box<dyn HttpHandler<Response = R> + Send + Sync>
    where
        H: HttpHandler<Response = R> + Send + Sync + 'static,
        R: Send + 'static;
}

/// HTTP client middleware.
///
/// Similar to Go Collector's `extensionmiddleware.HTTPClient`.
///
/// Implementations wrap HTTP round-trippers to add client-side functionality.
pub trait HttpClientMiddleware: Send + Sync {
    /// Wraps an HTTP client to add middleware functionality.
    ///
    /// Returns a modified client that includes the middleware behavior.
    fn wrap_client(&self, client: HttpClient) -> Result<HttpClient, ExtensionError>;
}

/// Simplified HTTP handler trait.
///
/// This is a simplified abstraction - real implementations would use
/// actual HTTP framework types (hyper, axum, etc.).
pub trait HttpHandler {
    type Response;

    /// Handles an HTTP request.
    fn handle(&self, request: HttpRequest) -> Self::Response;
}

/// Simplified HTTP request representation.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub uri: String,
    pub headers: std::collections::HashMap<String, String>,
    pub body: Vec<u8>,
}

/// Simplified HTTP client representation.
///
/// In practice, this would wrap `reqwest::Client` or similar.
#[derive(Clone)]
pub struct HttpClient {
    // Placeholder - would contain actual HTTP client
    _private: (),
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient {
    /// Creates a new HTTP client.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

/// gRPC server middleware.
///
/// Similar to Go Collector's `extensionmiddleware.GRPCServer`.
pub trait GrpcServerMiddleware: Send + Sync {
    /// Returns gRPC server interceptors/options.
    fn get_server_interceptors(&self) -> Result<Vec<GrpcInterceptor>, ExtensionError>;
}

/// gRPC client middleware.
///
/// Similar to Go Collector's `extensionmiddleware.GRPCClient`.
pub trait GrpcClientMiddleware: Send + Sync {
    /// Returns gRPC client interceptors/options.
    fn get_client_interceptors(&self) -> Result<Vec<GrpcInterceptor>, ExtensionError>;
}

/// Simplified gRPC interceptor representation.
///
/// In practice, this would use tonic's interceptor types.
#[derive(Clone)]
pub struct GrpcInterceptor {
    // Placeholder - would contain actual interceptor
    _private: (),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_client_default() {
        let _client = HttpClient::default();
    }

    #[test]
    fn test_http_request() {
        let req = HttpRequest {
            method: "GET".to_string(),
            uri: "/health".to_string(),
            headers: std::collections::HashMap::new(),
            body: vec![],
        };
        assert_eq!(req.method, "GET");
    }
}
