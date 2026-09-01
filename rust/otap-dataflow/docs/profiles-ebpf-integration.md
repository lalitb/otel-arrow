# Profiles eBPF Integration

This integration connects the OpenTelemetry eBPF profiler distribution to the
OTAP Dataflow Profiles implementation. It provides two complementary paths:

- a deterministic, unprivileged validation scenario suitable for normal CI;
- a pinned, environment-gated smoke test that loads the real eBPF profiler.

## Pinned upstream contract

The smoke test uses the official Collector distribution:

```text
otel/opentelemetry-collector-ebpf-profiler:0.159.0
sha256:90d6b6536ce0283d706f7e7b6c45f534c65b140ff6ec456c19385e50a7d12b8e
```

Collector release `0.159.0` includes
`go.opentelemetry.io/ebpf-profiler v0.0.202633`. Its Profiles service and
message definitions match the repository-pinned
`opentelemetry-proto` `v1development` API.

The profiler exports gzip-compressed OTLP/gRPC Profiles. The configured
receiver accepts messages up to 32 MiB because the profiler distribution can
produce requests larger than the receiver's normal 4 MiB default.
Collector `0.159.0` also requires the
`+service.profilesSupport` feature gate because the Profiles pipeline remains
alpha.

## Deterministic validation

The traffic generator emits bounded Profiles batches with:

- one shared mapping and stack per request;
- 16 shared locations and functions;
- eight samples per profile;
- profile, sample, mapping, and location attributes;
- stable graph shape and unique profile IDs per generated batch.

The validation pipeline is
`crates/validation/validation_pipelines/profiles-end-to-end.yaml`:

```text
synthetic Profiles
  -> OTLP/gRPC receiver
  -> sample filter with graph compaction and dense IDs
  -> reversible attribute transforms
  -> durable buffer
  -> OTAP/gRPC exporter
  -> validation OTAP receiver
  -> canonical OTLP reconstruction
  -> semantic-equivalence assertion
```

Run the scenario from `rust/otap-dataflow`:

```bash
RUSTFLAGS='--cfg validation_tests' \
  cargo test -p otel-arrow-dfe-validation \
  --lib validation_profiles_end_to_end -- --nocapture
```

This path does not load eBPF and does not require Docker or elevated
privileges.

## Real profiler smoke test

The real-profiler pipeline is defined in `configs/profiles-ebpf-smoke.yaml`. It
receives OTLP Profiles, compacts and validates the graph, persists it in the
durable buffer, reconstructs OTLP, and sends it to a second local OTLP receiver.
The sink writes bounded human-readable stack details and synchronized canonical
OTLP protobuf frames. The smoke test succeeds only after both receivers report
non-empty Profiles and both output artifacts are non-empty.
Both OTLP receivers wait for downstream completion. The terminal ACK follows
the file exporter's `sync_data`, while the source side is independently
protected by the durable buffer.

The launcher selects one of two collection modes:

- Native Linux Docker uses the host PID and network namespaces and profiles the
  workload running directly on the host.
- Docker Desktop under WSL uses the profiler's PID-namespace translation mode.
  It builds a static in-repository workload and runs it in a second,
  digest-pinned container sharing the profiler's PID namespace. This exercises
  the real eBPF tracer without claiming visibility into other WSL processes.
  The OTLP ingress port is bound to all WSL interfaces only for the duration of
  this mode so Docker Desktop's host gateway can reach it.

Run:

```bash
cd rust/otap-dataflow
tools/profiles/run-ebpf-smoke.sh
```

The script builds `df_engine`, the in-repository CPU workload, and the Profiles
file inspector. It starts the pipeline, launches the pinned profiler image,
waits for the `Attached sched monitor` marker, runs the workload, checks engine
metrics, validates every persisted frame, and prints the artifact paths.

The following environment variables are optional:

| Variable | Default | Purpose |
| --- | --- | --- |
| `OTEL_ARROW_EBPF_DURATION_SECONDS` | `15` | CPU workload duration, limited to 1-300 seconds |
| `OTEL_ARROW_EBPF_INGEST_PORT` | `14317` | Profiler-to-dataflow OTLP/gRPC port |
| `OTEL_ARROW_EBPF_SINK_PORT` | `14318` | Local reconstructed-OTLP sink port |
| `OTEL_ARROW_EBPF_ADMIN_PORT` | `18080` | Dataflow admin endpoint |
| `OTEL_ARROW_EBPF_SKIP_BUILD` | `0` | Reuse previously built debug binaries |
| `OTEL_ARROW_EBPF_PROFILER_IMAGE` | pinned image | Override only for explicit compatibility testing |
| `OTEL_ARROW_EBPF_WORKLOAD_IMAGE` | pinned Alpine image | Override the Docker Desktop sidecar workload image |
| `OTEL_ARROW_EBPF_OUTPUT_DIR` | `target/profiles-ebpf-smoke` | Absolute parent directory for private per-run artifacts |

## Inspecting persisted Profiles

Each run creates a mode-`0700` timestamped directory containing:

- `profiles-debug.txt`: mode-`0600` bounded profile, sample, attribute, stack,
  function, filename, and line output;
- `profiles-<core>-<generation>.otlp`: mode-`0600` versioned and checksummed
  canonical OTLP Profiles protobuf frames.

Reinspect the protobuf file with:

```bash
cargo run -p otel-arrow-dfe-validation \
  --example inspect_profiles_file -- \
  target/profiles-ebpf-smoke/<run>/profiles-1-0.otlp
```

The file format uses the `OTLPDF01` magic, signal identity, big-endian payload
length, CRC32, and exact protobuf bytes. The debug file is diagnostic text and
is not a replay format.

## Host requirements

Native host-wide mode requires:

- Linux on amd64 or arm64;
- kernel 5.10 or newer with eBPF support;
- a working Docker daemon;
- host PID and network namespaces;
- read-only access to `/sys/kernel/tracing`;
- root inside the profiler container;
- the `BPF`, `PERFMON`, `SYS_PTRACE`, `SYS_RESOURCE`,
  `DAC_READ_SEARCH`, `SYSLOG`, `CHECKPOINT_RESTORE`, and `IPC_LOCK`
  capabilities;
- unconfined seccomp and, when enabled on the host, AppArmor.

Host PID sharing exposes the host process table to the profiler. Run this test
only on a dedicated development or CI worker where system-wide profiling is
approved. Ordinary CI must use the deterministic validation path instead.

Docker Desktop users running under WSL must enable Docker integration for the
distribution. Docker Desktop's host PID namespace belongs to its Linux VM, not
the WSL distribution, so the script automatically uses container-scoped
sidecar mode instead of host-wide mode.

## Artifact and data handling

The repository does not vendor the profiler binary, image layers, upstream
coredumps, or a host profile capture. The profiler source is Apache-2.0, while
its embedded eBPF object carries GPL-2.0 terms. Referencing the pinned official
image avoids redistributing that artifact as part of this repository.

The smoke test profiles only the short-lived in-repository workload. Its output
directory is ignored build storage and must not be committed. Delete or retain
individual runs according to local privacy and retention policy. Any future
checked-in capture must be sanitized, redistributable, and accompanied by
provenance and privacy review.

## Known limitations

- The upstream profiler has no replay or synthetic mode that exercises its
  reporter without loading eBPF.
- The privileged smoke cannot run on ordinary unprivileged CI workers.
- Docker Desktop sidecar mode validates real eBPF collection only inside the
  shared profiler/workload PID namespace; use native Linux for host-wide
  process coverage.
- The smoke validates collection, graph conversion, processing, persistence,
  reconstruction, and non-empty delivery; deterministic semantic equivalence
  remains the responsibility of the unprivileged scenario.
- Go still lacks the Profiles Arrow codec needed for a cross-language version
  of this pipeline.
