# Geneva Exporter Integration Plan

## Objective
Produce a Geneva exporter component for the Rust-based OpenTelemetry collector in the `otel-arrow` repository by reusing the `geneva-uploader` crate from `opentelemetry-rust-contrib`. The exporter must support logs and traces, match the configuration surface of the Go `azuregigwarm` exporter, and integrate with existing pipeline/builder tooling.

## Current Components
- **geneva-uploader** (`opentelemetry-rust-contrib`): Provides `GenevaClient`, config client, payload encoders, batching, retries.
- **opentelemetry-exporter-geneva** (`opentelemetry-rust-contrib`): SDK-level exporters for logs/traces using `GenevaClient`.
- **azuregigwarm exporter** (`otel-azuregigwarm-exporter`): Go collector component exposing Geneva functionality via CGO bridge; defines configuration contract, batching, retry semantics, auth modes.
- **otel-arrow collector** (`otel-arrow/collector`): Go collector binary composed with builder; currently lacks Geneva exporter. Rust workspace (`otel-arrow/rust/otel-arrow-rust`, `otap-dataflow`) hosts OTAP/OTLP processing libraries but no collector exporter modules.

## Design Overview
1. **New Rust crate** `geneva-exporter` under `otel-arrow/rust`:
   - Depends on `geneva-uploader` via path dependency to checked-out `opentelemetry-rust-contrib`.
   - Exposes async exporter implementations for logs/traces conforming to the Rust collector pipeline interfaces (to be added).
2. **Collector Pipeline Integration**:
   - Extend Rust collector framework (likely in `otap-dataflow` or new `collector-components` crate) with exporter trait mirroring Go collector behavior (config parsing, lifecycle hooks, async `export`).
   - Provide config structs derived with `serde` using the same fields as the Go exporter (`endpoint`, `environment`, auth parameters, retry/batch settings).
3. **Encoding & Upload Path**:
   - Use `geneva_uploader::GenevaClient` directly.
   - Allow concurrency control for batch uploads similar to the SDK exporter.
   - Replicate batch retry/backoff logic from the Go exporter using `tokio` timers.
4. **Authentication**:
   - Map config to `GenevaClientConfig` supporting MSI, certificate, workload identity. Ensure secure handling of cert passwords (no logs).
5. **Collector Builder Updates**:
   - Surface the exporter through builder metadata (YAML module registration).
   - Update Go `components.go` generator inputs if the Rust exporter is wrapped or exposed via FFI; otherwise document how Rust collector loads it.
6. **FFI Considerations** (if needed):
   - If integration with Go collector remains necessary, create a minimal C-ABI shim for `GenevaClient` for reuse; otherwise keep purely Rust.

## Implementation Steps
1. **Workspace Preparation**
   - Add `geneva-exporter` crate to `otel-arrow/rust/Cargo.toml` workspace.
   - Introduce path dependency to `../../opentelemetry-rust-contrib/opentelemetry-exporter-geneva/geneva-uploader`.
2. **Configuration Module**
   - Define `GenevaExporterConfig` with serde attributes and validation (mirror Go `Config.Validate`).
   - Implement conversion into internal `GenevaClientConfig`.
3. **Exporter Logic**
   - Implement `LogsExporter` and `TracesExporter` structs with lifecycle methods (`start`, `shutdown`, `export`).
   - Use shared async executor abstraction already employed by collector (tokio-based).
   - Implement batch retry/backoff (configurable intervals, max retries, jitter).
4. **Pipeline Wiring**
   - Extend collector pipeline registry to accept new exporter (update config schema and pipeline builder to instantiate `LogsExporter`/`TracesExporter` based on config).
   - Ensure compatibility with both OTLP and OTAP data inputs (convert to OTLP protobuf before upload using existing pdata utilities).
5. **Testing**
   - Unit tests for config validation and retry logic.
  - Integration test using wiremock or test server similar to `geneva-uploader` tests to assert batch retries and auth flows.
   - Optional end-to-end test: pipeline that reads OTLP JSON, routes to new exporter with mocked HTTP endpoint.
6. **Documentation & Samples**
   - Update `otel-arrow/collector/README.md` with exporter description.
   - Provide example config under `otel-arrow/collector/examples/geneva/`.
   - Document environment setup (MSI vs certificate vs workload identity).
7. **CI & Build**
   - Ensure `cargo fmt`, `cargo clippy`, and existing lint settings pass for new crate.
   - If Go collector binary should expose the exporter, update Go builder configuration (metadata YAML) and regenerate `components.go`.
   - Add instructions to root `README.md` or dedicated doc referencing this plan.

## Testing Strategy
- **Unit Tests**: Config parsing, retry backoff, concurrency limits.
- **Mocked HTTP Tests**: Use `wiremock` like uploader crate to assert request formation.
- **Pipeline Smoke Test**: Run minimal collector pipeline in CI with env vars to ensure exporter loads and dispatches encoded batches to mock server.
- **Auth Mode Coverage**: MSI (mock), certificate (use temp cert), workload identity (fake token provider).

## Risks & Open Questions
- Determining the Rust collector abstraction for exporters: need alignment with ongoing `otap-dataflow` architecture.
- Handling of `geneva-uploader` updates: ensure version compatibility and avoid duplicate dependencies in workspace.
- Potential need for FFI layer if Go collector must still consume Rust exporter; clarify required deployment target.
- MSI and Workload Identity flows may need integration with Azure SDK crates already used by uploader; verify runtime requirements (systemd, IMDS access).
- Ensure licensing and internal usage constraints are met; exporter should remain Microsoft-internal per README guidance.
