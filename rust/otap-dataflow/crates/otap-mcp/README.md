# OTAP MCP Server

MCP (Model Context Protocol) server for the OTAP Dataflow Engine — the Rust-based OpenTelemetry collector.

Provides AI-assisted **component discovery**, **configuration validation**, **example configs**, and **runtime pipeline management** through the [Model Context Protocol](https://modelcontextprotocol.io/).

## Features

### Static Mode (always available)

| Tool / Resource | Description |
|---|---|
| `list_components` | Discover registered receivers, processors, and exporters |
| `validate_config` | Validate OTAP YAML pipeline configurations |
| `validate_component` | Validate a JSON config against a specific component |
| `generate_config` | Generate valid YAML from structured input (auto-wires DAG) |
| `get_component_schema` | Get JSON Schema for a component's configuration fields |
| `list_examples` | Browse example pipeline configurations |
| `get_example_config` | Retrieve specific example YAML configs |
| `components://*` | MCP resources for component listings |
| `examples://*` | MCP resources for example configs |
| `configure_pipeline` | Guided prompt for pipeline configuration |

### Runtime Mode (with `--admin-url`)

| Tool | Description |
|---|---|
| `get_pipeline_status` | Pipeline status from a running engine |
| `get_health` | Liveness + readiness health checks |
| `get_pipeline_health` | Per-pipeline liveness + readiness probes |
| `get_metrics` | Prometheus-format telemetry metrics |
| `get_aggregated_metrics` | Aggregated metrics with attribute grouping |
| `get_live_schema` | Live semantic conventions (SemConv) schema |
| `get_pipeline_detail` | Status of a specific pipeline |
| `shutdown_collector` | Graceful shutdown (destructive) |
| `dashboard://url` | MCP resource linking to the admin web dashboard |
| `debug_pipeline` | Guided prompt for troubleshooting |

## Usage

### Build

```bash
cargo build --release -p otap-df-mcp
```

### Run

```bash
# Static mode (component discovery + config tools) — stdio transport
otap-mcp --config-dir ./configs

# Static + runtime mode (also connects to a running collector)
otap-mcp --config-dir ./configs --admin-url http://127.0.0.1:8080

# Streamable HTTP transport (for remote MCP clients)
otap-mcp --transport streamable-http --http-bind 127.0.0.1:8090

# Full options
otap-mcp --config-dir ./configs --admin-url http://127.0.0.1:8080 \
         --transport streamable-http --http-bind 0.0.0.0:8090
```

### CLI Options

```
otap-mcp [OPTIONS]

OPTIONS:
  --admin-url <URL>         Admin API URL of a running df_engine
  --config-dir <DIR>        Path to example pipeline configs directory
  --transport <TRANSPORT>   Transport protocol: stdio (default) or streamable-http
  --http-bind <ADDR>        Bind address for HTTP transport (default: 127.0.0.1:8090)
```

## MCP Client Configuration

### Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "otap": {
      "command": "/path/to/otap-mcp",
      "args": ["--config-dir", "/path/to/configs"]
    }
  }
}
```

### VS Code (Copilot)

Add to `.vscode/mcp.json`:

```json
{
  "servers": {
    "otap": {
      "type": "stdio",
      "command": "/path/to/otap-mcp",
      "args": ["--config-dir", "/path/to/configs"]
    }
  }
}
```

### Remote (Streamable HTTP)

Start the server with HTTP transport, then point your MCP client to:

```
http://<host>:8090/mcp
```

## Architecture

```
┌─────────────────────────────────────────────┐
│           MCP Client (Claude, IDE)          │
└──────────────────┬──────────────────────────┘
                   │ MCP Protocol (stdio or HTTP)
                   ▼
┌─────────────────────────────────────────────┐
│          otap-df-mcp (MCP Server)           │
│                                             │
│  Resources    Tools         Prompts         │
│  ─────────    ─────         ───────         │
│  components   validate      configure       │
│  examples     list_*        debug           │
│  dashboard    get_schema                    │
│               generate                      │
│               get_status                    │
│               get_health                    │
│               get_metrics                   │
│               get_aggregated_metrics        │
│               get_live_schema               │
│               get_pipeline_health           │
│               shutdown                      │
│                                             │
│  Static Registry ──── Runtime Client ──┐    │
│  (linkme slices)      (reqwest HTTP)   │    │
└─────────────────────────────────────────────┘
        │                        │
        ▼                        ▼
  PipelineFactory          Admin HTTP API
  (compile-time)           (running df_engine)
```

## Design

Unlike the Go collector MCP server (`otelcol-mcp`) which fetches component metadata from GitHub at runtime, this server **discovers components at compile time** via the Rust `linkme` distributed slices pattern. This means:

- **Always in sync**: Component URNs match the binary exactly
- **Works offline**: No GitHub API dependency
- **Zero maintenance**: Adding a new component to the collector automatically registers it in the MCP server
- **Config schemas included**: Components that implement `schemars::JsonSchema` expose their full config schema through the `get_component_schema` tool

## License

Apache-2.0
