// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! MCP server implementation for the OTAP dataflow engine.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::resources::components;
use crate::resources::examples::ExampleStore;
use crate::runtime::{AdminClient, AggregateMetricsQuery};
use crate::tools::{generate, validate};

/// The OTAP MCP server handler.
#[derive(Clone)]
pub struct OtapMcpServer {
    example_store: Arc<ExampleStore>,
    admin_client: Option<Arc<AdminClient>>,
    admin_url: Option<String>,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for OtapMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtapMcpServer")
            .field("example_count", &self.example_store.len())
            .field("has_admin", &self.admin_client.is_some())
            .finish()
    }
}

impl OtapMcpServer {
    /// Creates a new MCP server instance.
    #[must_use]
    pub fn new(example_store: ExampleStore, admin_url: Option<&str>) -> Self {
        let admin_client = admin_url.map(|url| Arc::new(AdminClient::new(url)));
        Self {
            example_store: Arc::new(example_store),
            admin_client,
            admin_url: admin_url.map(|s| s.to_string()),
            tool_router: Self::tool_router(),
        }
    }

    fn require_admin(&self) -> Result<&AdminClient, ErrorData> {
        self.admin_client.as_deref().ok_or_else(|| {
            ErrorData::internal_error(
                "No admin URL configured. Start with --admin-url to enable runtime tools.",
                None,
            )
        })
    }

    /// Returns the dashboard URL if an admin URL is configured.
    fn dashboard_url(&self) -> Option<String> {
        self.admin_url
            .as_ref()
            .map(|url| format!("{url}/dashboard"))
    }
}

// -- Tool parameter types --------------------------------------------------

/// Parameters for the `list_components` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListComponentsParams {
    /// Component type: "receivers", "processors", "exporters", or "all".
    #[serde(default = "default_all")]
    pub component_type: String,
}

fn default_all() -> String {
    "all".to_string()
}

/// Parameters for the `validate_config` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ValidateConfigParams {
    /// The YAML configuration content to validate.
    pub yaml_content: String,
}

/// Parameters for the `get_example_config` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetExampleParams {
    /// Name of the example config file (e.g., "otap-otlp.yaml").
    pub name: String,
}

/// Parameters for the `get_pipeline_detail` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PipelineDetailParams {
    /// Pipeline group ID (e.g., "default").
    pub group_id: String,
    /// Pipeline ID (e.g., "main").
    pub pipeline_id: String,
}

/// Parameters for the `shutdown_collector` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShutdownParams {
    /// Whether to wait for shutdown to complete.
    #[serde(default)]
    pub wait: bool,
    /// Timeout in seconds when waiting.
    pub timeout_secs: Option<u64>,
}

/// Parameters for the `get_aggregated_metrics` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AggregateMetricsParams {
    /// Comma-separated attribute names to group by (e.g., "pipeline_id,node_id").
    pub attrs: Option<String>,
    /// Output format: "json" (default), "prometheus", "json_compact", "line_protocol".
    #[serde(default = "default_json")]
    pub format: String,
}

fn default_json() -> String {
    "json".to_string()
}

/// Parameters for the `get_pipeline_health` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PipelineHealthParams {
    /// Pipeline group ID (e.g., "default").
    pub group_id: String,
    /// Pipeline ID (e.g., "main").
    pub pipeline_id: String,
}

/// Parameters for the `get_component_schema` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetComponentSchemaParams {
    /// Component type: "receiver", "processor", or "exporter".
    pub component_type: String,
    /// Component name (e.g., "otlp", "batch", "console").
    pub component_name: String,
}

/// Parameters for the `validate_component` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ValidateComponentParams {
    /// Component type: "receiver", "processor", or "exporter".
    pub component_type: String,
    /// Component name (e.g., "otlp", "batch", "console").
    pub component_name: String,
    /// The JSON configuration to validate against this component's schema.
    pub config: serde_json::Value,
}

// -- Tool implementations --------------------------------------------------

#[tool_router]
impl OtapMcpServer {
    /// List registered OTAP dataflow engine components.
    #[tool(
        description = "List registered OTAP dataflow engine components. Filter by component_type: receivers, processors, exporters, or all."
    )]
    fn list_components(
        &self,
        Parameters(params): Parameters<ListComponentsParams>,
    ) -> Result<String, String> {
        let comp_type = params.component_type.as_str();
        let result = if comp_type == "all" {
            components::list_all_components()
        } else {
            components::list_components(comp_type).map_err(|e| e.to_string())?
        };
        serde_json::to_string_pretty(&result).map_err(|e| e.to_string())
    }

    /// Validate an OTAP dataflow engine YAML configuration.
    #[tool(
        description = "Validate an OTAP dataflow engine YAML config. Returns errors, warnings, and summary. \
        \n\nThe YAML must use OTAP format (NOT standard OpenTelemetry Collector format). \
        OTAP configs start with 'version: otel_dataflow/v1' and use groups/pipelines/nodes/connections structure. \
        \nExample minimal YAML:\n\
        version: otel_dataflow/v1\n\
        engine: {}\n\
        groups:\n\
          default:\n\
            pipelines:\n\
              main:\n\
                policies:\n\
                  channel_capacity:\n\
                    control: { node: 256, pipeline: 256 }\n\
                    pdata: 128\n\
                nodes:\n\
                  receiver:\n\
                    type: receiver:otlp\n\
                  exporter:\n\
                    type: exporter:console\n\
                connections:\n\
                  - from: receiver\n\
                    to: exporter"
    )]
    fn validate_config(
        &self,
        Parameters(params): Parameters<ValidateConfigParams>,
    ) -> Result<String, String> {
        let result = validate::validate_config(&params.yaml_content);
        serde_json::to_string_pretty(&result).map_err(|e| e.to_string())
    }

    /// List available example pipeline configurations.
    #[tool(description = "List available example OTAP pipeline configurations.")]
    fn list_examples(&self) -> Result<String, String> {
        let examples = self.example_store.list();
        serde_json::to_string_pretty(&examples).map_err(|e| e.to_string())
    }

    /// Get the YAML content of a specific example config.
    #[tool(description = "Get YAML content of a specific example pipeline config by name.")]
    fn get_example_config(
        &self,
        Parameters(params): Parameters<GetExampleParams>,
    ) -> Result<String, String> {
        self.example_store
            .get(&params.name)
            .map(|s| s.to_string())
            .ok_or_else(|| {
                let available: Vec<String> = self
                    .example_store
                    .list()
                    .iter()
                    .map(|e| e.name.clone())
                    .collect();
                format!(
                    "Example '{}' not found. Available: {}",
                    params.name,
                    available.join(", ")
                )
            })
    }

    /// Generate a valid OTAP pipeline YAML config from structured input.
    ///
    /// Specify receiver, processors, and exporter with their configs.
    /// The tool wires the DAG connections and validates the output automatically.
    #[tool(
        description = "Generate a valid OTAP pipeline YAML config from structured input. \
        \n\nRequired parameters:\n\
        - receiver: object with 'type' field (component URN), optional 'id' and 'config' (JSON string)\n\
        - exporter: object with 'type' field (component URN), optional 'id' and 'config' (JSON string)\n\
        \nOptional parameters:\n\
        - processors: array of objects, each with 'type' field, optional 'id' and 'config'\n\
        - group_id: string (default 'default')\n\
        - pipeline_id: string (default 'main')\n\
        - enable_admin: boolean (default false)\n\
        \nExample input:\n\
        {\n\
          \"receiver\": {\"type\": \"receiver:otlp\", \"config\": \"{\\\"protocols\\\":{\\\"grpc\\\":{\\\"listening_addr\\\":\\\"0.0.0.0:4317\\\"}}}\"},\n\
          \"processors\": [{\"type\": \"processor:batch\"}],\n\
          \"exporter\": {\"type\": \"exporter:otlp_grpc\", \"config\": \"{\\\"grpc_endpoint\\\":\\\"http://backend:4317\\\"}\"}\n\
        }\n\
        \nMinimal example (no config, no processors):\n\
        {\"receiver\": {\"type\": \"receiver:otlp\"}, \"exporter\": {\"type\": \"exporter:console\"}}"
    )]
    fn generate_config(
        &self,
        Parameters(params): Parameters<generate::GenerateConfigInput>,
    ) -> Result<String, String> {
        let yaml = generate::generate_config(&params)?;

        // Validate the generated config
        let validation = validate::validate_config(&yaml);
        if !validation.valid {
            let errors: Vec<String> = validation
                .messages
                .iter()
                .map(|m| m.message.clone())
                .collect();
            return Err(format!(
                "Generated config has validation errors:\n{}\n\n--- Generated YAML ---\n{}",
                errors.join("\n"),
                yaml
            ));
        }

        Ok(yaml)
    }

    /// Get the JSON Schema for a specific component's configuration.
    ///
    /// Returns the full JSON Schema describing the expected configuration
    /// fields, types, defaults, and constraints for a given component.
    #[tool(
        description = "Get JSON Schema for a component's configuration. Specify component_type (receiver, processor, exporter) and component_name (e.g., otlp, batch, console)."
    )]
    fn get_component_schema(
        &self,
        Parameters(params): Parameters<GetComponentSchemaParams>,
    ) -> Result<String, String> {
        match components::get_component_schema(&params.component_type, &params.component_name)? {
            Some(schema) => serde_json::to_string_pretty(&schema).map_err(|e| e.to_string()),
            None => Ok(format!(
                "No JSON Schema available for {} '{}'. \
                 The component is registered but its config type does not yet expose a schema.",
                params.component_type, params.component_name
            )),
        }
    }

    /// Validate a JSON configuration against a specific component's config schema.
    ///
    /// Checks that the provided JSON config is valid for the given component type and name.
    #[tool(
        description = "Validate a JSON config against a specific component. Specify component_type (receiver, processor, exporter), component_name (e.g., otlp, batch), and config (JSON object)."
    )]
    fn validate_component(
        &self,
        Parameters(params): Parameters<ValidateComponentParams>,
    ) -> Result<String, String> {
        match components::validate_component_config(
            &params.component_type,
            &params.component_name,
            &params.config,
        ) {
            Ok(()) => Ok(format!(
                "✓ Configuration is valid for {} '{}'.",
                params.component_type, params.component_name
            )),
            Err(e) => Ok(format!(
                "✗ Configuration is invalid for {} '{}':\n{}",
                params.component_type, params.component_name, e
            )),
        }
    }

    /// Get pipeline status from a running OTAP dataflow engine.
    #[tool(
        description = "Get pipeline status from a running OTAP dataflow engine. Requires --admin-url."
    )]
    async fn get_pipeline_status(&self) -> Result<String, String> {
        let client = self.require_admin().map_err(|e| e.message.to_string())?;
        let status = client.get_status().await.map_err(|e| e.to_string())?;
        serde_json::to_string_pretty(&status).map_err(|e| e.to_string())
    }

    /// Get health status (liveness + readiness) from a running engine.
    #[tool(
        description = "Get health (liveness + readiness) from a running OTAP dataflow engine. Requires --admin-url."
    )]
    async fn get_health(&self) -> Result<String, String> {
        let client = self.require_admin().map_err(|e| e.message.to_string())?;
        let health = client.get_health().await.map_err(|e| e.to_string())?;
        serde_json::to_string_pretty(&health).map_err(|e| e.to_string())
    }

    /// Get Prometheus-format metrics from a running engine.
    #[tool(
        description = "Get Prometheus-format metrics from a running OTAP dataflow engine. Requires --admin-url."
    )]
    async fn get_metrics(&self) -> Result<String, String> {
        let client = self.require_admin().map_err(|e| e.message.to_string())?;
        client.get_metrics().await.map_err(|e| e.to_string())
    }

    /// Get status of a specific pipeline.
    #[tool(
        description = "Get status of a specific pipeline by group_id and pipeline_id. Requires --admin-url."
    )]
    async fn get_pipeline_detail(
        &self,
        Parameters(params): Parameters<PipelineDetailParams>,
    ) -> Result<String, String> {
        let client = self.require_admin().map_err(|e| e.message.to_string())?;
        let detail = client
            .get_pipeline_detail(&params.group_id, &params.pipeline_id)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string_pretty(&detail).map_err(|e| e.to_string())
    }

    /// Request graceful shutdown of a running OTAP dataflow engine. DESTRUCTIVE.
    #[tool(
        description = "Graceful shutdown of a running OTAP dataflow engine. DESTRUCTIVE. Requires --admin-url.",
        annotations(destructive_hint = true, read_only_hint = false)
    )]
    async fn shutdown_collector(
        &self,
        Parameters(params): Parameters<ShutdownParams>,
    ) -> Result<String, String> {
        let client = self.require_admin().map_err(|e| e.message.to_string())?;
        let response = client
            .shutdown(params.wait, params.timeout_secs)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string_pretty(&response).map_err(|e| e.to_string())
    }

    /// Get aggregated metrics from a running engine, grouped by attributes.
    #[tool(
        description = "Get aggregated metrics from a running OTAP dataflow engine. \
        Supports grouping by attributes (e.g., 'pipeline_id,node_id') and multiple \
        output formats (json, prometheus, json_compact, line_protocol). Requires --admin-url."
    )]
    async fn get_aggregated_metrics(
        &self,
        Parameters(params): Parameters<AggregateMetricsParams>,
    ) -> Result<String, String> {
        let client = self.require_admin().map_err(|e| e.message.to_string())?;
        let query = AggregateMetricsQuery {
            attrs: params.attrs,
            format: Some(params.format),
        };
        client
            .get_aggregated_metrics(&query)
            .await
            .map_err(|e| e.to_string())
    }

    /// Get the live semantic conventions schema from a running engine.
    #[tool(
        description = "Get the live semantic conventions (SemConv) schema from a running OTAP \
        dataflow engine. Shows which metrics are currently registered and their metadata. \
        Requires --admin-url."
    )]
    async fn get_live_schema(&self) -> Result<String, String> {
        let client = self.require_admin().map_err(|e| e.message.to_string())?;
        let schema = client.get_live_schema().await.map_err(|e| e.to_string())?;
        serde_json::to_string_pretty(&schema).map_err(|e| e.to_string())
    }

    /// Get health status of a specific pipeline (liveness + readiness).
    #[tool(
        description = "Get health (liveness + readiness) of a specific pipeline by group_id \
        and pipeline_id. More granular than get_health which checks the whole engine. \
        Requires --admin-url."
    )]
    async fn get_pipeline_health(
        &self,
        Parameters(params): Parameters<PipelineHealthParams>,
    ) -> Result<String, String> {
        let client = self.require_admin().map_err(|e| e.message.to_string())?;
        let health = client
            .get_pipeline_health(&params.group_id, &params.pipeline_id)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string_pretty(&health).map_err(|e| e.to_string())
    }
}

// -- ServerHandler ----------------------------------------------------------

#[tool_handler]
impl ServerHandler for OtapMcpServer {
    fn get_info(&self) -> ServerInfo {
        let has_runtime = self.admin_client.is_some();
        let mode = if has_runtime {
            "static + runtime"
        } else {
            "static only"
        };

        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_instructions(format!(
            "OTAP Dataflow Engine MCP Server ({mode}). \
             Provides component discovery, config validation, example configs{}.",
            if has_runtime {
                ", and runtime pipeline management"
            } else {
                ". Start with --admin-url to enable runtime tools"
            }
        ))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let mut resources = vec![
            Annotated::new(
                RawResource::new("components://all", "All Components")
                    .with_description("All registered receivers, processors, and exporters")
                    .with_mime_type("application/json"),
                None,
            ),
            Annotated::new(
                RawResource::new("components://receivers", "Receivers")
                    .with_description("Registered receiver components")
                    .with_mime_type("application/json"),
                None,
            ),
            Annotated::new(
                RawResource::new("components://processors", "Processors")
                    .with_description("Registered processor components")
                    .with_mime_type("application/json"),
                None,
            ),
            Annotated::new(
                RawResource::new("components://exporters", "Exporters")
                    .with_description("Registered exporter components")
                    .with_mime_type("application/json"),
                None,
            ),
        ];

        if !self.example_store.is_empty() {
            resources.push(Annotated::new(
                RawResource::new("examples://list", "Example Configs")
                    .with_description(format!(
                        "{} example pipeline configurations",
                        self.example_store.len()
                    ))
                    .with_mime_type("application/json"),
                None,
            ));
        }

        if let Some(ref url) = self.admin_url {
            resources.push(Annotated::new(
                RawResource::new("dashboard://url", "Admin Dashboard")
                    .with_description(format!(
                        "Web dashboard URL for live monitoring at {url}/dashboard"
                    ))
                    .with_mime_type("text/plain"),
                None,
            ));
        }

        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        let uri = &request.uri;
        let content = match uri.as_str() {
            "components://all" => {
                let all = components::list_all_components();
                serde_json::to_string_pretty(&all)
                    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
            }
            "components://receivers" | "components://processors" | "components://exporters" => {
                let comp_type = uri.strip_prefix("components://").unwrap_or("");
                let comps = components::list_components(comp_type)
                    .map_err(|e| ErrorData::internal_error(e, None))?;
                serde_json::to_string_pretty(&comps)
                    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
            }
            "examples://list" => {
                let examples = self.example_store.list();
                serde_json::to_string_pretty(&examples)
                    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
            }
            _ if uri.starts_with("example://") => {
                let name = uri.strip_prefix("example://").unwrap_or("");
                self.example_store
                    .get(name)
                    .map(|s| s.to_string())
                    .ok_or_else(|| {
                        ErrorData::resource_not_found(
                            format!("Example config '{name}' not found"),
                            None,
                        )
                    })?
            }
            "dashboard://url" => self.dashboard_url().ok_or_else(|| {
                ErrorData::internal_error(
                    "No admin URL configured. Start with --admin-url to enable the dashboard.",
                    None,
                )
            })?,
            _ => {
                return Err(ErrorData::resource_not_found(
                    format!("Unknown resource: {uri}"),
                    None,
                ));
            }
        };

        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            content,
            uri.as_str(),
        )]))
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        let prompts = vec![
            Prompt::new(
                "configure_pipeline",
                Some("Guided workflow for configuring an OTAP dataflow pipeline"),
                Some(vec![
                    PromptArgument::new("requirements")
                        .with_description("Description of what the pipeline should do")
                        .with_required(false),
                ]),
            ),
            Prompt::new(
                "debug_pipeline",
                Some("Guided workflow for debugging OTAP pipeline issues"),
                Some(vec![
                    PromptArgument::new("issue")
                        .with_description("Description of the issue being experienced")
                        .with_required(false),
                ]),
            ),
        ];

        Ok(ListPromptsResult::with_all_items(prompts))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, ErrorData> {
        match request.name.as_str() {
            "configure_pipeline" => {
                let user_input = request
                    .arguments
                    .as_ref()
                    .and_then(|args| args.get("requirements"))
                    .map(|s| format!("\n\nUser requirements: {s}"))
                    .unwrap_or_default();

                Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                    PromptMessageRole::User,
                    format!(
                        "{}{}",
                        crate::prompts::configure::configure_pipeline_prompt(),
                        user_input
                    ),
                )])
                .with_description("Guided OTAP pipeline configuration"))
            }
            "debug_pipeline" => {
                let user_input = request
                    .arguments
                    .as_ref()
                    .and_then(|args| args.get("issue"))
                    .map(|s| format!("\n\nIssue description: {s}"))
                    .unwrap_or_default();

                Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                    PromptMessageRole::User,
                    format!(
                        "{}{}",
                        crate::prompts::debug::debug_pipeline_prompt(),
                        user_input
                    ),
                )])
                .with_description("Guided OTAP pipeline debugging"))
            }
            name => Err(ErrorData::invalid_params(
                format!("Unknown prompt: {name}"),
                None,
            )),
        }
    }
}
