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

The real-host pipeline is defined in `configs/profiles-ebpf-smoke.yaml`. It
receives OTLP Profiles, compacts and validates the graph, persists it in the
durable buffer, reconstructs OTLP, and sends it to a second local OTLP receiver.
The smoke test succeeds only after both receivers report non-empty Profiles.

Run:

```bash
cd rust/otap-dataflow
tools/profiles/run-ebpf-smoke.sh
```

The script builds `df_engine` and the in-repository CPU workload, starts the
pipeline, launches the pinned profiler image, waits for the
`Attached sched monitor` marker, runs the workload, and checks engine metrics.

The following environment variables are optional:

| Variable | Default | Purpose |
| --- | --- | --- |
| `OTEL_ARROW_EBPF_DURATION_SECONDS` | `15` | CPU workload duration, limited to 1-300 seconds |
| `OTEL_ARROW_EBPF_INGEST_PORT` | `14317` | Profiler-to-dataflow OTLP/gRPC port |
| `OTEL_ARROW_EBPF_SINK_PORT` | `14318` | Local reconstructed-OTLP sink port |
| `OTEL_ARROW_EBPF_ADMIN_PORT` | `18080` | Dataflow admin endpoint |
| `OTEL_ARROW_EBPF_SKIP_BUILD` | `0` | Reuse previously built debug binaries |
| `OTEL_ARROW_EBPF_PROFILER_IMAGE` | pinned image | Override only for explicit compatibility testing |

## Host requirements

The real smoke test intentionally fails fast unless the host provides:

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
distribution before running the smoke test.

## Artifact and data handling

The repository does not vendor the profiler binary, image layers, upstream
coredumps, or a host profile capture. The profiler source is Apache-2.0, while
its embedded eBPF object carries GPL-2.0 terms. Referencing the pinned official
image avoids redistributing that artifact as part of this repository.

The smoke test profiles only the short-lived in-repository workload and does
not save exported Profiles. A future checked-in capture must be sanitized,
redistributable, and accompanied by provenance and privacy review.

## Known limitations

- The upstream profiler has no replay or synthetic mode that exercises its
  reporter without loading eBPF.
- The privileged smoke cannot run on ordinary unprivileged CI workers.
- The smoke validates collection, graph conversion, processing, persistence,
  reconstruction, and non-empty delivery; deterministic semantic equivalence
  remains the responsibility of the unprivileged scenario.
- Go still lacks the Profiles Arrow codec needed for a cross-language version
  of this pipeline.
