// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Embedded MCP server wiring for the admin HTTP server.
//!
//! This module mounts the MCP (Model Context Protocol) streamable-HTTP endpoint
//! onto the admin axum router, or binds it on a separate TCP listener when a
//! dedicated `bind_address` is configured.
//!
//! # Phase 1 — loopback runtime tools
//!
//! Runtime tools (status, health, metrics, shutdown) connect back to the admin
//! API over HTTP via [`AdminClient`][otap_df_mcp::runtime::AdminClient].  The
//! loopback request is handled by the same tokio runtime on the admin thread,
//! which is correct and safe — the async runtime yields while awaiting the
//! response, allowing the loopback request to be serviced.
//!
//! Phase 2 will replace this with direct handle access (no HTTP round-trip).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use otap_df_config::engine::McpSettings;
use otap_df_mcp::resources::examples::ExampleStore;
use otap_df_mcp::server::OtapMcpServer;
use otap_df_telemetry::{otel_info, otel_warn};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::error::Error;

/// Mount the MCP server at `/mcp` on the given router.
///
/// The `admin_url` is used by the MCP runtime tools to call back into the
/// admin HTTP API (Phase 1 loopback approach).
pub(crate) fn mount(router: Router, config: &McpSettings, admin_url: &str) -> Router {
    let server = build_server(config, admin_url);
    let service = build_service(server);

    otel_info!(
        "admin.mcp.mounted",
        message = "MCP server mounted on admin router at /mcp"
    );

    router.route("/mcp", axum::routing::any_service(service))
}

/// Bind a separate TCP listener for MCP and serve it concurrently with the
/// admin server.  The MCP server shuts down when `cancel` is triggered.
///
/// Returns an error if the MCP bind address is invalid or cannot be bound.
pub(crate) async fn spawn_separate(
    config: &McpSettings,
    mcp_bind: &str,
    admin_url: &str,
    cancel: CancellationToken,
) -> Result<(), Error> {
    let addr = mcp_bind.parse::<SocketAddr>().map_err(|e| {
        let details = format!("{e}");
        otel_warn!(
            "admin.mcp.parse_address_failed",
            bind_address = mcp_bind,
            error = details.as_str(),
            message = "Failed to parse MCP server bind address"
        );
        Error::InvalidBindAddress {
            bind_address: mcp_bind.to_owned(),
            details,
        }
    })?;

    let listener = TcpListener::bind(&addr).await.map_err(|e| {
        let addr_str = addr.to_string();
        let details = format!("{e}");
        otel_warn!(
            "admin.mcp.bind_failed",
            bind_address = addr_str.as_str(),
            error = details.as_str(),
            message = "Failed to bind MCP server"
        );
        Error::BindFailed {
            addr: addr_str,
            details,
        }
    })?;

    let addr_str = addr.to_string();
    otel_info!(
        "admin.mcp.start",
        bind_address = addr_str.as_str(),
        message = "MCP HTTP server listening on separate port"
    );

    let server = build_server(config, admin_url);
    let service = build_service(server);
    let app = Router::new().route("/mcp", axum::routing::any_service(service));

    let _handle = tokio::task::spawn_local(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                cancel.cancelled().await;
            })
            .await;
        if let Err(e) = result {
            let err_str = e.to_string();
            otel_warn!(
                "admin.mcp.server_error",
                error = err_str.as_str(),
                message = "MCP HTTP server encountered an error"
            );
        }
    });

    Ok(())
}

/// Build an `OtapMcpServer` with the given config and admin loopback URL.
fn build_server(config: &McpSettings, admin_url: &str) -> OtapMcpServer {
    let example_store = load_example_store(config);
    OtapMcpServer::new(example_store, Some(admin_url))
}

/// Wrap an `OtapMcpServer` in a `StreamableHttpService` for axum routing.
fn build_service(
    server: OtapMcpServer,
) -> StreamableHttpService<OtapMcpServer, LocalSessionManager> {
    let session_manager = Arc::new(LocalSessionManager::default());
    let mcp_config = StreamableHttpServerConfig::default();
    StreamableHttpService::new(move || Ok(server.clone()), session_manager, mcp_config)
}

/// Load example configs from the configured directory, falling back to empty.
fn load_example_store(config: &McpSettings) -> ExampleStore {
    if let Some(ref dir) = config.example_dir {
        match ExampleStore::from_directory(dir) {
            Ok(store) => store,
            Err(e) => {
                let dir_str = dir.display().to_string();
                let err_str = e.to_string();
                otel_warn!(
                    "admin.mcp.example_dir_failed",
                    dir = dir_str.as_str(),
                    error = err_str.as_str(),
                    message =
                        "Failed to load MCP example configs from directory; using empty store"
                );
                ExampleStore::empty()
            }
        }
    } else {
        ExampleStore::empty()
    }
}
