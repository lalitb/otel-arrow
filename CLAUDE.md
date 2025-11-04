# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

OpenTelemetry Protocol with Apache Arrow (OTel-Arrow) is a columnar encoding protocol for OpenTelemetry telemetry data. This repository contains reference implementations in both Go (Phase 1) and Rust (Phase 2), implementing the OTAP (OpenTelemetry Protocol with Apache Arrow) protocol.

**Key Concepts:**
- **OTLP**: Standard OpenTelemetry Protocol (row-oriented, stateless, protobuf-based)
- **OTAP**: OpenTelemetry Protocol with Apache Arrow (column-oriented, stateful, Arrow IPC-based)
- **OTAP Records**: Arrow RecordBatch representation of telemetry organized in "star schema"
- **Phase 1**: Go implementation integrated into OpenTelemetry Collector-Contrib
- **Phase 2**: Rust dataflow engine with Arrow-first architecture

## Common Commands

### Go Development

All Go commands run from the repository root:

```bash
# Run all tests across all Go modules
make test

# Build all Go modules
make build

# Update dependencies across all modules
make gotidy

# Format Go code
make fmt

# Run everything (tidy + test + build)
make all

# Build the test collector (otelarrowcol)
make otelarrowcol
# Binary output: ./bin/otelarrowcol

# Build Docker image for otelarrowcol
make docker-otelarrowcol
```

**Note**: This repository uses multiple `go.mod` files. The Makefile handles iterating over all modules.

### Rust Development (OTAP Dataflow Engine)

Navigate to `rust/otap-dataflow` for Rust commands:

```bash
cd rust/otap-dataflow

# Build the entire workspace
cargo build --workspace

# Run all tests
cargo test --workspace

# Run a single test
cargo test -p <crate-name> <test-name>

# Pre-commit validation (format, lint, test, docs)
cargo xtask check

# Run the dataflow engine with a config
cargo run -- -p configs/<config-file>.yaml

# Run benchmarks
cargo bench --workspace

# Build Docker image
docker build --build-context otel-arrow=../../ -f Dockerfile -t df_engine .
```

**Important Rust Settings:**
- Minimum Rust version: 1.86.0
- Workspace uses Rust edition 2024
- Strict lints enforced: `unsafe_code = "deny"`, `unwrap_used = "deny"`

### Testing Individual Components

```bash
# Test a specific crate
cargo test -p otap-df-engine
cargo test -p otap-df-otap

# Run tests with output
cargo test --workspace -- --nocapture

# Run with nextest (if available)
cargo nextest run --workspace
```

## Architecture Overview

### Repository Structure

```
/
├── go/                      # Phase 1 Go reference implementation
│   ├── pkg/otel/arrow_record/  # Producer/Consumer for OTAP streams
│   └── tools/               # Analysis and testing utilities
├── rust/                    # Rust implementations
│   ├── otap-dataflow/       # Phase 2 dataflow engine (PRIMARY)
│   │   ├── crates/          # Modular crate architecture
│   │   │   ├── engine/      # Core DAG infrastructure
│   │   │   ├── otap/        # OTAP pipeline components
│   │   │   ├── controller/  # Multi-core orchestration
│   │   │   ├── config/      # YAML configuration model
│   │   │   ├── pdata/       # Pipeline data abstractions
│   │   │   ├── channel/     # Custom MPSC/MPMC queues
│   │   │   ├── admin/       # HTTP admin portal
│   │   │   ├── state/       # State machine support
│   │   │   └── telemetry/   # NUMA-aware metrics
│   │   └── src/main.rs      # df_engine CLI binary
│   ├── otel-arrow-rust/     # Low-level OTAP/OTLP conversion
│   ├── experimental/        # Query abstraction & engine
│   └── beaubourg/           # Alternative pipeline framework
├── collector/               # Test collector implementation
├── proto/                   # Protobuf definitions
├── docs/                    # Architecture documentation
└── tools/                   # Performance testing tools
```

### Two-Phase Architecture

**Phase 1 (Go - Production):**
- Integrated into OpenTelemetry Collector-Contrib
- Components: `otelarrowexporter`, `otelarrowreceiver`
- Row-oriented internal representation (pdata)
- Converts at boundaries: pdata ↔ OTAP stream

**Phase 2 (Rust - Active Development):**
- Arrow-first columnar architecture end-to-end
- Thread-per-core design with zero-copy conversions
- Primary data type: OTAP records (Arrow RecordBatch)
- Direct conversion: OTLP bytes ↔ OTAP records (no intermediate objects)

### OTAP Dataflow Engine Design Principles

From `.cursor/rules/single-threaded-async-runtime.mdc`:
- **Target single-threaded async runtime** (tokio LocalRuntime)
- **Declare async traits as `?Send`** with `!Send` implementations when practical
- **Avoid synchronization primitives** - thread-per-core isolation
- **Optimize for performance** - zero-copy where possible
- **Avoid unbounded channels** - all queues have capacity limits
- **Minimize dependencies** - careful about dependency bloat

### Data Flow Patterns

```
OTLP Bytes ←→ OTAP Records ←→ OTAP Stream (Arrow IPC)
     ↑              ↑                 ↑
  Protobuf    RecordBatch[]      Wire Format
```

**Key Data Types:**
- **`OtapPdata`**: Central pipeline data type with context + payload
  - Payload has two equivalent representations: OTLP bytes OR OTAP records
  - Signal-specific: Logs, Metrics, or Traces
- **OTAP Records**: 4 RecordBatches for Logs, similar for Metrics/Traces
  - Primary table (logs/spans/metrics)
  - Attributes table (N-to-1 via foreign keys)
  - Resource attributes table
  - Scope attributes table

### Pipeline Components

Located in `rust/otap-dataflow/crates/otap/`:

**Receivers** (data sources):
- OTLP gRPC receiver
- OTAP gRPC receiver (streaming)
- Syslog/CEF receiver
- Fake data generator

**Processors** (transformations):
- Batch processor (Arrow-native batching)
- Retry processor (exponential backoff)
- Attributes processor (column manipulation)
- Signal router (route by signal type)
- Debug processor

**Exporters** (data sinks):
- OTLP gRPC exporter
- OTAP gRPC exporter (streaming)
- Parquet exporter
- Performance exporter (metrics collection)
- Noop/Error exporters (testing)

## Configuration

Pipelines are defined in YAML as directed acyclic graphs (DAGs). Examples in `rust/otap-dataflow/configs/`:

```yaml
nodes:
  receiver_node:
    kind: receiver
    plugin_urn: "urn:otel:otap:receiver"
    config: {...}
    out_ports:
      default:
        destinations: [processor_node]
        dispatch_strategy: round_robin
  processor_node:
    kind: processor
    plugin_urn: "urn:otel:batch:processor"
    in_port: {}
    out_ports: {...}
  exporter_node:
    kind: exporter
    plugin_urn: "urn:otel:otap:exporter"
    in_port: {}
```

**Dispatch Strategies:**
- `round_robin`: Load balance across destinations
- `broadcast`: Send to all destinations
- `clone_to_first`: Avoid copy on single destination

## Development Guidelines

### Working with Rust Code

1. **Respect the single-threaded design**: Use `?Send` traits and `!Send` types when possible
2. **No unwrap()**: Lint enforces proper error handling with `Result`/`Option`
3. **No println!/dbg!**: Use the logging/telemetry infrastructure
4. **Document everything**: `missing_docs = "deny"` requires doc comments
5. **No unsafe code**: `unsafe_code = "deny"` unless absolutely necessary with justification

### Zero-Copy Patterns

The codebase uses "views" for zero-copy access:
- **OTLP bytes → OTAP records**: Views interpret bytes directly, streaming to Arrow arrays
- **OTAP records → OTLP bytes**: Views read RecordBatch, encode directly to bytes
- Files ending in `_views.rs` implement these abstractions

### Effect Handler Pattern

Components interact with the runtime via effect handlers:
- **Network effects**: gRPC clients/servers abstracted
- **Clock effects**: Time/timers for testing
- **Ack/Nack effects**: Message acknowledgment routing

Two variants:
- `NotSendEffectHandler`: Preferred, uses `Rc<RefCell<T>>`
- `SendEffectHandler`: For `Send`-requiring libraries (Tonic)

### Message Passing

**Control Plane** (`NodeControlMsg`):
- `Ack` / `Nack`: Response routing
- `Timer`: Scheduled events
- `Stop` / `Reconfigure`: Lifecycle management

**Data Plane**:
- `OtapPdata` flowing through channels
- Separate control and data queues per node

### Adding New Components

1. Implement the appropriate trait: `Receiver`, `Processor`, or `Exporter`
2. Register with `linkme` for plugin discovery
3. Define configuration structs in `crates/config/`
4. Add integration tests in component's module
5. Update `configs/` with example pipeline using the component

## Testing

### Unit Tests

```bash
# Run all tests
cargo test --workspace

# Run tests for specific crate
cargo test -p otap-df-engine

# Run specific test with output
cargo test test_name -- --nocapture
```

### Integration Tests

Located in each crate's `tests/` directory. Use fixtures from `crates/otap/fixtures.rs`.

### Benchmarks

```bash
cd rust/otap-dataflow
cargo bench --workspace

# Specific benchmark
cargo bench -p otap-df-otap batch_processor
```

Continuous benchmarks tracked in CI with `cargobench` label on PRs.

## Important Files & Patterns

### Key Entry Points

- **Go Producer**: `go/pkg/otel/arrow_record/producer.go`
- **Go Consumer**: `go/pkg/otel/arrow_record/consumer.go`
- **Rust Engine**: `rust/otap-dataflow/crates/engine/lib.rs`
- **Rust Pipeline Factory**: `rust/otap-dataflow/crates/otap/lib.rs`
- **Rust Main Binary**: `rust/otap-dataflow/src/main.rs`
- **Controller**: `rust/otap-dataflow/crates/controller/lib.rs`

### Common File Naming

- `*_views.rs`: Zero-copy view abstractions
- `*_processor.rs`: Pipeline processor implementations
- `*_encoder.rs`: OTLP → OTAP conversion
- `*_consumer.rs`: OTAP → OTLP conversion
- `lib.rs`: Crate root with public API
- `mod.rs`: Module organization

### Documentation

- **Data Model**: `docs/data_model.md` - Arrow schema mappings
- **OTAP Basics**: `docs/otap_basics.md` - Protocol fundamentals
- **Benchmarks**: `docs/benchmarks.md` - Performance comparisons
- **Project Phases**: `docs/project-phases.md` - Roadmap and milestones

## External Dependencies

### Go
- OpenTelemetry Collector (pdata types)
- Apache Arrow Go bindings
- gRPC and Protocol Buffers

### Rust
- **arrow-rs** (v56.1): Apache Arrow implementation
- **tokio** (v1.46): Async runtime with LocalRuntime support
- **tonic** (v0.14): gRPC framework
- **prost** (v0.14): Protocol Buffers
- **parquet** (v56.1): Parquet file format
- **weaver**: Semantic convention code generation

## Performance Characteristics

**Compression vs OTLP+ZSTD:**
- Moderate batches (100-1000 items): 30-50% improvement
- Large batches (10k+ items): 20-40% improvement
- Multi-variate metrics: Even higher gains

**Memory:**
- Thread-per-core avoids synchronization overhead
- Zero-copy conversions reduce allocations
- Bounded channels prevent memory bloat

**Throughput:**
- Columnar format enables vectorized operations
- Dictionary encoding reduces repetitive data
- Persistent connections amortize schema overhead

## Known Limitations & Patterns

1. **No thread synchronization**: Components must be designed for single-threaded execution
2. **Generic over PData**: The engine itself is data-type agnostic
3. **Stateful protocol**: OTAP requires persistent gRPC connections for dictionary reuse
4. **Schema evolution**: Must handle Arrow schema changes between batches
5. **NUMA awareness**: Controller assigns pipelines to specific CPU cores

## CI/CD

- `go-ci.yml`: Go tests across all modules
- `rust-ci.yml`: Rust compilation, tests, clippy, rustfmt
- `rust-audit.yml`: Security audits with cargo-audit
- `rust-bench.yml`: Performance benchmarks (triggered by label)
- Code coverage uploaded to codecov.io

## Resources

- **Slack**: #otel-arrow on CNCF Slack
- **Meetings**: Bi-weekly Thursdays at 8AM PT (OpenTelemetry calendar)
- **OTEP**: OpenTelemetry Enhancement Proposal 0156 (columnar encoding)
- **Collector-Contrib**: otelarrowexporter/otelarrowreceiver components
