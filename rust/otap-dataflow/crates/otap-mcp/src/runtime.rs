// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Runtime client for the OTAP dataflow engine admin HTTP API.
//!
//! Wraps the admin endpoints exposed by a running `df_engine` instance,
//! providing typed access to pipeline status, health, metrics, and shutdown.

use crate::error::Error;
use serde::{Deserialize, Serialize};

/// Client for the OTAP dataflow engine admin HTTP API.
#[derive(Debug, Clone)]
pub struct AdminClient {
    base_url: String,
    client: reqwest::Client,
}

/// Response from the `/status` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponse {
    /// Timestamp when the status was generated.
    pub generated_at: String,
    /// Pipeline statuses keyed by pipeline identifier.
    pub pipelines: serde_json::Value,
}

/// Response from the `/livez` or `/readyz` endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResponse {
    /// Probe type (`livez` or `readyz`).
    pub probe: String,
    /// Probe status (`ok` or `failed`).
    pub status: String,
    /// Timestamp when the probe was generated.
    pub generated_at: String,
    /// List of failing pipeline conditions, if any.
    #[serde(default)]
    pub failing: Vec<serde_json::Value>,
}

/// Response from the `POST /pipeline-groups/shutdown` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShutdownResponse {
    /// Shutdown status.
    pub status: String,
    /// Any errors encountered during shutdown.
    #[serde(default)]
    pub errors: Option<Vec<String>>,
    /// Duration of the shutdown in milliseconds.
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

/// Health check result combining liveness and readiness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    /// Liveness probe result.
    pub liveness: ProbeResponse,
    /// Readiness probe result.
    pub readiness: ProbeResponse,
}

/// Pipeline groups status response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineGroupsStatusResponse {
    /// Timestamp when the status was generated.
    pub generated_at: String,
    /// Pipeline statuses.
    pub pipelines: serde_json::Value,
}

/// Per-pipeline health combining liveness and readiness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineHealthStatus {
    /// Pipeline group ID.
    pub group_id: String,
    /// Pipeline ID.
    pub pipeline_id: String,
    /// Liveness probe result.
    pub liveness: ProbeResponse,
    /// Readiness probe result.
    pub readiness: ProbeResponse,
}

/// Query parameters for aggregated metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AggregateMetricsQuery {
    /// Comma-separated attribute names to group by.
    #[serde(default)]
    pub attrs: Option<String>,
    /// Output format: json, prometheus, json_compact, line_protocol.
    #[serde(default)]
    pub format: Option<String>,
}

impl AdminClient {
    /// Creates a new admin client targeting the given base URL.
    ///
    /// # Arguments
    ///
    /// * `base_url` - Base URL of the admin HTTP API (e.g., `http://127.0.0.1:8080`)
    #[must_use]
    pub fn new(base_url: &str) -> Self {
        let base_url = base_url.trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        Self { base_url, client }
    }

    /// Gets the overall pipeline status from `/status`.
    pub async fn get_status(&self) -> Result<StatusResponse, Error> {
        let url = format!("{}/status", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::AdminConnectionError {
                details: e.to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(Error::AdminApiError {
                status: resp.status().as_u16(),
                details: resp.text().await.unwrap_or_default(),
            });
        }

        resp.json().await.map_err(|e| Error::AdminApiError {
            status: 0,
            details: format!("failed to parse response: {e}"),
        })
    }

    /// Gets the health status by querying both `/livez` and `/readyz`.
    pub async fn get_health(&self) -> Result<HealthStatus, Error> {
        let livez_url = format!("{}/livez", self.base_url);
        let readyz_url = format!("{}/readyz", self.base_url);

        let (livez_resp, readyz_resp) =
            futures::try_join!(self.fetch_probe(&livez_url), self.fetch_probe(&readyz_url),)?;

        Ok(HealthStatus {
            liveness: livez_resp,
            readiness: readyz_resp,
        })
    }

    /// Gets Prometheus-format metrics from `/telemetry/metrics`.
    pub async fn get_metrics(&self) -> Result<String, Error> {
        let url = format!("{}/telemetry/metrics", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::AdminConnectionError {
                details: e.to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(Error::AdminApiError {
                status: resp.status().as_u16(),
                details: resp.text().await.unwrap_or_default(),
            });
        }

        resp.text().await.map_err(|e| Error::AdminApiError {
            status: 0,
            details: format!("failed to read response: {e}"),
        })
    }

    /// Gets the status of a specific pipeline.
    pub async fn get_pipeline_detail(
        &self,
        group_id: &str,
        pipeline_id: &str,
    ) -> Result<serde_json::Value, Error> {
        let url = format!(
            "{}/pipeline-groups/{}/pipelines/{}/status",
            self.base_url, group_id, pipeline_id
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::AdminConnectionError {
                details: e.to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(Error::AdminApiError {
                status: resp.status().as_u16(),
                details: resp.text().await.unwrap_or_default(),
            });
        }

        resp.json().await.map_err(|e| Error::AdminApiError {
            status: 0,
            details: format!("failed to parse response: {e}"),
        })
    }

    /// Gets the pipeline groups status from `/pipeline-groups/status`.
    pub async fn get_pipeline_groups_status(&self) -> Result<PipelineGroupsStatusResponse, Error> {
        let url = format!("{}/pipeline-groups/status", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::AdminConnectionError {
                details: e.to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(Error::AdminApiError {
                status: resp.status().as_u16(),
                details: resp.text().await.unwrap_or_default(),
            });
        }

        resp.json().await.map_err(|e| Error::AdminApiError {
            status: 0,
            details: format!("failed to parse response: {e}"),
        })
    }

    /// Gets aggregated metrics from `/telemetry/metrics/aggregate`.
    pub async fn get_aggregated_metrics(
        &self,
        query: &AggregateMetricsQuery,
    ) -> Result<String, Error> {
        let mut url = format!("{}/telemetry/metrics/aggregate", self.base_url);
        let mut params = Vec::new();
        if let Some(ref attrs) = query.attrs {
            params.push(format!("attrs={attrs}"));
        }
        if let Some(ref fmt) = query.format {
            params.push(format!("format={fmt}"));
        }
        if !params.is_empty() {
            url = format!("{url}?{}", params.join("&"));
        }

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::AdminConnectionError {
                details: e.to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(Error::AdminApiError {
                status: resp.status().as_u16(),
                details: resp.text().await.unwrap_or_default(),
            });
        }

        resp.text().await.map_err(|e| Error::AdminApiError {
            status: 0,
            details: format!("failed to read response: {e}"),
        })
    }

    /// Gets the live semantic conventions schema from `/telemetry/live-schema`.
    pub async fn get_live_schema(&self) -> Result<serde_json::Value, Error> {
        let url = format!("{}/telemetry/live-schema", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::AdminConnectionError {
                details: e.to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(Error::AdminApiError {
                status: resp.status().as_u16(),
                details: resp.text().await.unwrap_or_default(),
            });
        }

        resp.json().await.map_err(|e| Error::AdminApiError {
            status: 0,
            details: format!("failed to parse response: {e}"),
        })
    }

    /// Gets per-pipeline health by querying pipeline-specific liveness and readiness probes.
    pub async fn get_pipeline_health(
        &self,
        group_id: &str,
        pipeline_id: &str,
    ) -> Result<PipelineHealthStatus, Error> {
        let livez_url = format!(
            "{}/pipeline-groups/{}/pipelines/{}/livez",
            self.base_url, group_id, pipeline_id
        );
        let readyz_url = format!(
            "{}/pipeline-groups/{}/pipelines/{}/readyz",
            self.base_url, group_id, pipeline_id
        );

        let (livez_resp, readyz_resp) =
            futures::try_join!(self.fetch_probe(&livez_url), self.fetch_probe(&readyz_url),)?;

        Ok(PipelineHealthStatus {
            group_id: group_id.to_string(),
            pipeline_id: pipeline_id.to_string(),
            liveness: livez_resp,
            readiness: readyz_resp,
        })
    }

    /// Requests a graceful shutdown via `POST /pipeline-groups/shutdown`.
    pub async fn shutdown(
        &self,
        wait: bool,
        timeout_secs: Option<u64>,
    ) -> Result<ShutdownResponse, Error> {
        let mut url = format!("{}/pipeline-groups/shutdown", self.base_url);
        let mut params = Vec::new();
        if wait {
            params.push("wait=true".to_string());
        }
        if let Some(timeout) = timeout_secs {
            params.push(format!("timeout_secs={timeout}"));
        }
        if !params.is_empty() {
            url = format!("{url}?{}", params.join("&"));
        }

        let resp =
            self.client
                .post(&url)
                .send()
                .await
                .map_err(|e| Error::AdminConnectionError {
                    details: e.to_string(),
                })?;

        if !resp.status().is_success() {
            return Err(Error::AdminApiError {
                status: resp.status().as_u16(),
                details: resp.text().await.unwrap_or_default(),
            });
        }

        resp.json().await.map_err(|e| Error::AdminApiError {
            status: 0,
            details: format!("failed to parse response: {e}"),
        })
    }

    /// Helper to fetch a probe endpoint.
    async fn fetch_probe(&self, url: &str) -> Result<ProbeResponse, Error> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| Error::AdminConnectionError {
                details: e.to_string(),
            })?;

        // Probes may return non-200 (e.g. 503 for not-ready) but still have a body.
        resp.json().await.map_err(|e| Error::AdminApiError {
            status: 0,
            details: format!("failed to parse probe response: {e}"),
        })
    }
}
