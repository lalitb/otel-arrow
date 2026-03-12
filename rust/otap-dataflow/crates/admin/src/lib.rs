// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! HTTP server for exposing admin endpoints.

mod dashboard;
pub mod error;
mod health;
#[cfg(feature = "mcp")]
mod mcp;
mod pipeline;
mod pipeline_group;
mod telemetry;

use axum::Router;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;

use crate::error::Error;
use otap_df_config::engine::HttpAdminSettings;
use otap_df_engine::control::PipelineAdminSender;
use otap_df_state::store::ObservedStateHandle;
use otap_df_telemetry::registry::TelemetryRegistryHandle;
use otap_df_telemetry::{otel_info, otel_warn};

/// Shared state for the HTTP admin server.
#[derive(Clone)]
struct AppState {
    /// The observed state store for querying the current state of the entire system.
    observed_state_store: ObservedStateHandle,

    /// The metrics registry for querying current metrics.
    metrics_registry: TelemetryRegistryHandle,

    /// The control message senders for controlling pipelines.
    ctrl_msg_senders: Arc<Mutex<Vec<Arc<dyn PipelineAdminSender>>>>,
}

/// Run the admin HTTP server until shutdown is requested.
pub async fn run(
    config: HttpAdminSettings,
    observed_store: ObservedStateHandle,
    ctrl_msg_senders: Vec<Arc<dyn PipelineAdminSender>>,
    metrics_registry: TelemetryRegistryHandle,
    cancel: CancellationToken,
) -> Result<(), Error> {
    let app_state = AppState {
        observed_state_store: observed_store,
        metrics_registry,
        ctrl_msg_senders: Arc::new(Mutex::new(ctrl_msg_senders)),
    };

    let app = Router::new()
        .merge(health::routes())
        .merge(telemetry::routes())
        .merge(pipeline_group::routes())
        .merge(pipeline::routes())
        .merge(dashboard::routes())
        .layer(ServiceBuilder::new())
        .with_state(app_state);

    // Conditionally wire the MCP server when compiled with --features mcp.
    //
    // When mcp.bind_address is None: mount /mcp on the admin router (same port).
    // When mcp.bind_address is Some: leave admin router unchanged; a separate
    // listener is spawned below after the admin listener is bound.
    #[cfg(feature = "mcp")]
    let admin_url = admin_loopback_url(&config.bind_address);

    #[cfg(feature = "mcp")]
    let app = if let Some(mcp_cfg) = config.mcp.as_ref().filter(|m| m.enabled && m.bind_address.is_none()) {
        mcp::mount(app, mcp_cfg, &admin_url)
    } else {
        app
    };

    // Warn if MCP is enabled in config but the binary lacks the mcp feature.
    #[cfg(not(feature = "mcp"))]
    if config.mcp.as_ref().is_some_and(|m| m.enabled) {
        otel_warn!(
            "admin.mcp_not_compiled",
            message = "engine.http_admin.mcp.enabled is true but this binary was not compiled \
                       with --features mcp; the MCP server will not start"
        );
    }

    // Parse the configured bind address.
    let addr = config.bind_address.parse::<SocketAddr>().map_err(|e| {
        let details = format!("{e}");
        otel_warn!(
            "endpoint.parse_address_failed",
            bind_address = config.bind_address.as_str(),
            error = details.as_str(),
            message = "Failed to parse admin server bind address"
        );
        Error::InvalidBindAddress {
            bind_address: config.bind_address.clone(),
            details,
        }
    })?;

    // Bind the TCP listener.
    let listener = TcpListener::bind(&addr).await.map_err(|e| {
        let addr_str = addr.to_string();
        let details = format!("{e}");
        otel_warn!(
            "endpoint.bind_failed",
            bind_address = addr_str.as_str(),
            error = details.as_str(),
            message = "Failed to bind admin server"
        );
        Error::BindFailed {
            addr: addr_str,
            details,
        }
    })?;

    let addr_str = addr.to_string();
    otel_info!(
        "endpoint.start",
        bind_address = addr_str.as_str(),
        message = "Admin HTTP server listening"
    );

    // Spawn the MCP server on a separate port if configured.
    #[cfg(feature = "mcp")]
    if let Some(mcp_cfg) = config.mcp.as_ref().filter(|m| m.enabled) {
        if let Some(ref mcp_bind) = mcp_cfg.bind_address {
            mcp::spawn_separate(mcp_cfg, mcp_bind, &admin_url, cancel.clone()).await?;
        }
    }

    // Start serving requests, with graceful shutdown on signal.
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            cancel.cancelled().await;
        })
        .await
        .map_err(|e| Error::ServerError {
            addr: addr.to_string(),
            details: format!("{e}"),
        })
}

#[cfg(feature = "mcp")]
/// Construct a loopback URL from the admin bind address.
///
/// Normalizes wildcard addresses (`0.0.0.0`, `::`) to `127.0.0.1` / `[::1]`
/// so the embedded MCP client can connect back to the admin API.
fn admin_loopback_url(bind_address: &str) -> String {
    match bind_address.parse::<SocketAddr>() {
        Ok(addr) => {
            let port = addr.port();
            if addr.ip().is_unspecified() {
                // Wildcard address: pick the matching loopback interface.
                // IPv4 (0.0.0.0) → 127.0.0.1; IPv6 (::) → [::1].
                match addr {
                    SocketAddr::V4(_) => format!("http://127.0.0.1:{port}"),
                    SocketAddr::V6(_) => format!("http://[::1]:{port}"),
                }
            } else {
                format!("http://{addr}")
            }
        }
        Err(_) => format!("http://{bind_address}"),
    }
}

#[cfg(all(test, feature = "mcp"))]
mod tests {
    use super::*;

    #[test]
    fn loopback_url_ipv4_wildcard() {
        assert_eq!(admin_loopback_url("0.0.0.0:8080"), "http://127.0.0.1:8080");
    }

    #[test]
    fn loopback_url_ipv6_wildcard() {
        assert_eq!(admin_loopback_url("[::]:8080"), "http://[::1]:8080");
    }

    #[test]
    fn loopback_url_explicit_ipv4() {
        assert_eq!(
            admin_loopback_url("127.0.0.1:8080"),
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn loopback_url_explicit_ipv6() {
        assert_eq!(admin_loopback_url("[::1]:8080"), "http://[::1]:8080");
    }

    #[test]
    fn loopback_url_non_loopback_ip() {
        assert_eq!(
            admin_loopback_url("192.168.1.10:8080"),
            "http://192.168.1.10:8080"
        );
    }
}
