# Geneva Exporter (OTAP Dataflow)

This document describes how to enable the Geneva exporter in the OTAP dataflow engine using the shared `geneva-uploader` crate.

## Prerequisites

- Valid Geneva account, namespace, and region.
- Authentication material:
  - **Managed Identity**: collector must run on Azure resource with assigned identity.
  - **Workload Identity**: Kubernetes cluster configured for federated identity.
  - **Certificate**: PKCS#12 bundle (.p12) with client certificate and private key.
- Geneva configuration service access (requires `config_major_version` aligned with target environment).

## Configuration Schema

All fields live under the exporter node’s `config` section.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `endpoint` | string | yes | Geneva config service base URI. |
| `environment` | string | yes | Geneva environment moniker. |
| `account` | string | yes | Geneva account name. |
| `namespace` | string | yes | Geneva namespace. |
| `region` | string | yes | Geneva region (e.g., `eastus`). |
| `config_major_version` | uint | yes | Config version advertised by Geneva. Must be > 0. |
| `tenant` | string | yes | Tenant identifier included in uploader headers. |
| `role_name` | string | yes | Geneva role name. |
| `role_instance` | string | yes | Unique identifier for the collector instance. |
| `auth_method` | enum | yes | One of `managed_identity`, `workload_identity`, `certificate`. |
| `identity_resource` | string | depends | Required for identity-based auth methods. Specifies the Azure resource audience. |
| `managed_identity_client_id` | string | optional | Client ID for user-assigned identity. |
| `cert_path` | string | required when `auth_method = certificate` | Absolute path to PKCS#12 bundle. |
| `cert_password` | string | optional | Password for the PKCS#12 bundle (empty when not set). |
| `max_concurrent_uploads` | integer (>0) | optional (default 4) | Parallel upload concurrency. |
| `upload_queue_size` | integer (>0) | optional (default 256) | Buffered queue size for encoded batches. |
| `batch_retry.enabled` | bool | optional (default true) | Turn retry logic on/off. |
| `batch_retry.max_retries` | uint | optional (default 3) | Attempts per batch. |
| `batch_retry.initial_interval` | duration | optional (default `100ms`) | Initial backoff. |
| `batch_retry.max_interval` | duration | optional (default `5s`) | Max backoff. |
| `batch_retry.multiplier` | float | optional (default `2.0`) | Exponential multiplier. |

Durations use the `humantime` textual format (`"250ms"`, `"10s"`, etc.).

## Example Pipeline

`configs/otlp-geneva.yaml` demonstrates a simple OTLP receiver feeding the Geneva exporter:

```yaml
settings:
  default_pipeline_ctrl_msg_channel_size: 100
  default_node_ctrl_msg_channel_size: 100
  default_pdata_channel_size: 100

nodes:
  receiver:
    kind: receiver
    plugin_urn: "urn:otel:otlp:receiver"
    out_ports:
      out_port:
        destinations: [exporter]
        dispatch_strategy: round_robin
    config:
      listening_addr: "0.0.0.0:4317"

  exporter:
    kind: exporter
    plugin_urn: "urn:otel:geneva:exporter"
    config:
      endpoint: "https://ingestion.monitor.azure.com"
      environment: "prod"
      account: "your-account"
      namespace: "your-namespace"
      region: "eastus"
      config_major_version: 1
      tenant: "your-tenant"
      role_name: "collector-role"
      role_instance: "collector-instance"
      auth_method: "managed_identity"
      identity_resource: "https://monitor.azure.com/"
      max_concurrent_uploads: 8
      upload_queue_size: 512
      batch_retry:
        enabled: true
        max_retries: 5
        initial_interval: "250ms"
        max_interval: "10s"
        multiplier: 2.0
```

Update the identity or certificate fields to match the deployment environment.

## Go Collector Integration

To expose the Geneva exporter from the Go-based `otelarrowcol` build:

1. Ensure the `otel-azuregigwarm-exporter` repo is checked out alongside `otel-arrow`.
2. Update `collector/otelarrowcol-build.yaml` to include:
   - An exporter entry  
     `gomod: github.com/open-telemetry/otel-azuregigwarm-exporter/exporter/azuregigwarmexporter v0.1.0`
   - A local replace directive  
     `github.com/open-telemetry/otel-azuregigwarm-exporter => ../../otel-azuregigwarm-exporter`
3. Build the Go collector with the builder (e.g. `ocb --config collector/otelarrowcol-build.yaml`).
4. Refer to `otel-azuregigwarm-exporter/README.md` for FFI bridge compilation and sample Collector configs.

This keeps the Go collector aligned with the Rust exporter feature set.

## Logging & Telemetry

- Internal pdata metrics are exposed via the OTAP telemetry registry (`exporter.pdata.*` counters).
- Transient upload failures increment `logs_failed`/`traces_failed`, while successful batches increment `logs_exported`/`traces_exported`.
- Enable `tracing` subscribers to capture retry warnings (`geneva_exporter` logger).

## Testing Recommendations

1. **Mock Endpoint**: Use `wiremock` to emulate Geneva ingestion for CI/unit tests.
2. **Certificate Flow**: Generate temporary PKCS#12 bundle using `openssl` / `rcgen` (matches uploader tests).
3. **Identity Flow**: When running locally, configure `AZURE_CLIENT_ID` and use `azure-identity` environment credentials.

## Troubleshooting

- `AuthInfoNotFound`: the Geneva configuration service returned no ingestion info; verify account/namespace/region.
- `RequestFailed 403`: identity lacks required permissions; ensure role assignment in Geneva.
- `Certificate` errors: confirm path and password, and ensure PKCS#12 includes full chain.

For more context on the original design, see `docs/geneva_exporter_plan.md`.
