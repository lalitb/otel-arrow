# OTAP Dataflow Extension System

This crate provides an extensible extension system for the OTAP dataflow
pipeline engine, following the same patterns used by the OpenTelemetry Go
Collector.

## Overview

Extensions are shared components that provide auxiliary functionality to
pipeline components (receivers, processors, exporters). They don't participate
directly in data pipelines but provide services like:

- **Authentication**: Provide credentials for outgoing requests or validate
  incoming requests
- **Middleware**: Wrap HTTP/gRPC handlers and clients
- **Capabilities**: Health checks, config watching, etc.

## Architecture

```text
+-------------------------------------------------------------+
|                      EXTENSIONS                             |
|  +--------------+  +--------------+  +--------------+       |
|  | Bearer Token |  |  Azure Auth  |  |  Basic Auth  |  ...  |
|  +------+-------+  +------+-------+  +------+-------+       |
+---------+-----------------+-----------------+---------------+
          |                 |                 |
          v                 v                 v
+-------------------------------------------------------------+
|                   ExtensionHost                             |
|         (provides extensions to components)                 |
+-------------------------------------------------------------+
          |
          v
+-------------------------------------------------------------+
|              Pipeline Components                            |
|    (Receivers, Processors, Exporters access extensions)     |
+-------------------------------------------------------------+
```

## Usage

### Defining an Extension

```rust
use otap_df_extension::{Extension, ExtensionFactory, EXTENSION_FACTORIES, auth::ClientAuth};
use linkme::distributed_slice;
use std::any::Any;
use std::sync::Arc;

struct MyAuthExtension {
    token: String,
}

impl Extension for MyAuthExtension {
    fn name(&self) -> &'static str {
        "my-auth"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl ClientAuth for MyAuthExtension {
    fn get_request_metadata(&self) -> Result<HashMap<String, String>, ExtensionError> {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {}", self.token));
        Ok(headers)
    }
}

// Register the extension factory
#[distributed_slice(EXTENSION_FACTORIES)]
static MY_AUTH_FACTORY: ExtensionFactory = ExtensionFactory {
    name: "urn:otel:extension:auth:my-auth",
    create: |config| {
        let token = config["token"].as_str().unwrap_or_default();
        Ok(Arc::new(MyAuthExtension { token: token.to_string() }))
    },
};
```

### Using Extensions in Components

```rust
use otap_df_extension::host::{ExtensionHost, ExtensionRef};

// In component configuration
struct MyExporterConfig {
    auth: Option<ExtensionRef>,
}

// In component implementation
fn setup_client(host: &ExtensionHost, config: &MyExporterConfig) -> Result<Client, Error> {
    if let Some(auth_ref) = &config.auth {
        let auth = host.get_client_auth(&auth_ref.id)?;
        let headers = auth.get_request_metadata()?;
        // Apply headers to client...
    }
    // ...
}
```

### Configuration

```yaml
extensions:
  - id: my-auth
    extension_urn: "urn:otel:extension:auth:bearer-token"
    config:
      token: "my-secret-token"

exporters:
  my-exporter:
    endpoint: "https://api.example.com"
    auth:
      id: my-auth  # Reference to extension
```

## Extension Traits

### Authentication (`auth` module)

| Trait | Purpose |
|-------|---------|
| `ServerAuth` | Authenticate incoming requests (receivers) |
| `ClientAuth` | Provide credentials for outgoing requests (exporters) |
| `CredentialProvider` | Generic credential provider (cloud storage, etc.) |

### Middleware (`middleware` module)

| Trait | Purpose |
|-------|---------|
| `HttpServerMiddleware` | Wrap HTTP handlers (receivers) |
| `HttpClientMiddleware` | Wrap HTTP clients (exporters) |
| `GrpcServerMiddleware` | Add gRPC server interceptors |
| `GrpcClientMiddleware` | Add gRPC client interceptors |

## Comparison with Go Collector

| Go Collector | Rust (this crate) |
|--------------|-------------------|
| `extension.Extension` | `Extension` trait |
| `extensionauth.Server` | `auth::ServerAuth` |
| `extensionauth.HTTPClient` | `auth::ClientAuth` |
| `extensionmiddleware.HTTPServer` | `middleware::HttpServerMiddleware` |
| `component.Host.GetExtensions()` | `ExtensionHost::get_extensions()` |
| `configauth.Config` | `host::ExtensionRef` |

## Built-in Extensions

- `urn:otel:extension:auth:bearer-token` - Bearer token authentication

## Adding New Extensions

1. Create a new module in `src/impls/`
2. Implement the `Extension` trait
3. Implement relevant capability traits (`ClientAuth`, `ServerAuth`, etc.)
4. Register using `#[distributed_slice(EXTENSION_FACTORIES)]`

See `src/impls/bearer_token.rs` for a complete example.
