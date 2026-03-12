// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! CLI entry point for the OTAP MCP server.

use clap::{Parser, ValueEnum};
use otap_df_contrib_nodes as _;
use rmcp::ServiceExt;
use std::path::PathBuf;

use otap_df_mcp::resources::examples::{self, ExampleStore};
use otap_df_mcp::server::OtapMcpServer;

/// Transport protocol for the MCP server.
#[derive(Debug, Clone, ValueEnum)]
enum Transport {
    /// Standard I/O transport (default, for subprocess-based MCP clients).
    Stdio,
    /// Streamable HTTP transport with SSE (for remote MCP clients).
    StreamableHttp,
}

/// MCP server for the OTAP Dataflow Engine.
///
/// Provides AI-assisted component discovery, configuration validation,
/// and runtime management of the OTAP dataflow engine through the
/// Model Context Protocol (MCP).
#[derive(Parser, Debug)]
#[command(
    name = "otap-mcp",
    author,
    version,
    about = "MCP server for the OTAP Dataflow Engine",
    long_about = "Model Context Protocol server providing AI-assisted discovery, \
                  configuration, and operational management of the OTAP dataflow engine."
)]
struct Args {
    /// URL of a running OTAP dataflow engine admin API.
    ///
    /// When provided, enables runtime tools (status, health, metrics, shutdown).
    /// Example: http://127.0.0.1:8080
    #[arg(long, value_name = "URL")]
    admin_url: Option<String>,

    /// Path to the directory containing example pipeline configurations.
    ///
    /// If not specified, attempts to find a `configs/` directory relative to
    /// the current working directory.
    #[arg(long, value_name = "DIR")]
    config_dir: Option<PathBuf>,

    /// Transport protocol to use.
    #[arg(long, value_enum, default_value_t = Transport::Stdio)]
    transport: Transport,

    /// Bind address for the streamable HTTP transport.
    ///
    /// Only used when --transport=streamable-http.
    #[arg(long, default_value = "127.0.0.1:8090", value_name = "ADDR")]
    http_bind: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Load example configs
    let example_store = if let Some(ref dir) = args.config_dir {
        ExampleStore::from_directory(dir)?
    } else if let Some(default_dir) = examples::default_config_dir() {
        ExampleStore::from_directory(&default_dir).unwrap_or_else(|_| ExampleStore::empty())
    } else {
        ExampleStore::empty()
    };

    // Create the MCP server
    let server = OtapMcpServer::new(example_store, args.admin_url.as_deref());

    match args.transport {
        Transport::Stdio => {
            // Start the server on stdio transport
            let service = server.serve(rmcp::transport::stdio()).await?;
            let _quit_reason = service.waiting().await?;
        }
        Transport::StreamableHttp => {
            use rmcp::transport::streamable_http_server::{
                StreamableHttpServerConfig, StreamableHttpService,
                session::local::LocalSessionManager,
            };

            let bind_addr: std::net::SocketAddr = args
                .http_bind
                .parse()
                .map_err(|e| format!("Invalid --http-bind address '{}': {e}", args.http_bind))?;

            // Note: The MCP protocol runs over HTTP at http://{bind_addr}/mcp
            let session_manager = std::sync::Arc::new(LocalSessionManager::default());
            let config = StreamableHttpServerConfig::default();

            let service =
                StreamableHttpService::new(move || Ok(server.clone()), session_manager, config);

            let app = axum::Router::new().route("/mcp", axum::routing::any_service(service));

            let listener = tokio::net::TcpListener::bind(bind_addr).await?;
            axum::serve(listener, app).await?;
        }
    }

    Ok(())
}
