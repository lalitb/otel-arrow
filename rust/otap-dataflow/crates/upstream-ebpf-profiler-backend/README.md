# Upstream eBPF Profiler Backend

This experimental crate investigates a Rust userspace control plane for the
existing OpenTelemetry eBPF Profiler C/eBPF kernel artifact. It is intentionally
separate from a DFE receiver and from the independent profiler implementation.

The native path, demonstrated on Linux amd64 frame-pointer workloads, is:

```text
external upstream C/eBPF ELF
    -> compatibility validator
    -> temporary thread-scoped kernel-stack discovery
    -> audited native-probe map preparation
    -> optional Aya loader
    -> bounded process and frame-pointer metadata
    -> checked event decoder
    -> bounded aggregation
    -> owned ProfileSnapshot
```

No Go runtime, Go/Rust FFI, child profiler process, Collector, loopback OTLP,
gRPC, protobuf intermediate, Arrow array, or DFE type participates in that path.

## Status

The non-privileged milestone is complete and tested:

- Rust-based inspection of the unmodified amd64 and arm64 artifacts;
- SHA-256 and architecture verification;
- deterministic program, map, global, inner-map, and tail-call inventory;
- pinned amd64 and arm64 compatibility manifests;
- typed compatibility failures;
- checked unaligned event decoding;
- bounded file reads, process mappings, aggregation bytes/cardinality, snapshot
  handoff, diagnostics, and explicit planning estimates;
- an optional Aya-based native loader that compiles without changing the
  workspace's default dependency or toolchain requirements.

The ordinary shell still lacks BPF/perf capabilities, but explicitly authorized
temporary containers now provide live evidence: both native chains initialize,
one- and two-CPU sampling produce owned snapshots, workload stacks reach 41
native frames, and real startup rollback/repeated shutdown restore descriptors.
Overlayfs handling and kernel-mode register recovery are covered as well.
Generation guards reject stale/unverifiable records, and shutdown has bounded
deadline-aware drain/finalization work. These are not universal hard real-time
or all-kernel production guarantees. A separate exact-source Go comparison
confirms native address agreement while documenting capability and
resource-budget differences.
See [FEASIBILITY.md](FEASIBILITY.md).

The current unwind implementation is deliberately narrow. It emits the
upstream direct frame-pointer command for executable ELF ranges. This can
produce meaningful stacks only for workloads compiled with reliable frame
pointers. It does not port the upstream `.eh_frame`, `.debug_frame`, or Go
`pclntab` extractor.

## Pinned compatibility

The primary amd64 manifest targets:

- upstream commit:
  `06ea040c39d3d17bc1534a5dcc044368caf48782`;
- amd64 artifact SHA-256:
  `0f186e7f99d2b544fc69ec02834ebfb902e09e4ab89eda51436a4ad8a2ec7282`;
- compatibility label: `upstream-06ea040c-amd64-v1`.

The GPL eBPF object is not vendored. Supply its external path at runtime.
The arm64 inventory is also pinned, but live arm64 profiling is not supported
by evidence from this experiment.

## Inspect an artifact without root

From `rust/otap-dataflow`:

```bash
cargo run \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --example inspect -- \
  /absolute/path/to/tracer.ebpf.amd64 \
  upstream-06ea040c-amd64-v1 \
  06ea040c39d3d17bc1534a5dcc044368caf48782 \
  --check
```

The output is deterministic JSON; `--check` compares the pinned manifest before
printing it. Omit `--check` only when deliberately generating an inventory for
review. For arm64 use `upstream-06ea040c-arm64-v1`.

The native loader prepares six map bindings in an owned copy of the pinned
object before loading it. This mirrors the upstream Go userspace's native
probe-map associations; the external file and all executable sections remain
unchanged. To inspect the original/prepared fingerprints and exact bindings
without writing or loading an eBPF object:

```bash
cargo run \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --example prepare -- \
  /absolute/path/to/tracer.ebpf.amd64
```

Use `prepare <object> analysis` to inspect the separate, narrowly scoped
analysis-only instruction patch. Its exact bytes, fingerprints, and required
TID/namespace settings are documented in [LICENSES.md](LICENSES.md).

Host prerequisites and configured memory estimates are also non-privileged:

```bash
cargo run \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --example preflight -- \
  /absolute/path/to/tracer.ebpf.amd64
```

To verify the pinned external object:

```bash
UPSTREAM_EBPF_OBJECT=/absolute/path/to/tracer.ebpf.amd64 \
  cargo test \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --test artifact_compatibility
```

## Build the optional native path

Aya is feature-gated so the normal workspace build does not require root,
Clang, LLVM, bpftool, libbpf, libelf development headers, or a BPF toolchain:

```bash
cargo check \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --features native
```

An explicitly authorized host with the required capabilities can run:

```bash
OTEL_ARROW_EBPF_PRIVILEGED_TEST=1 \
UPSTREAM_EBPF_OBJECT=/absolute/path/to/tracer.ebpf.amd64 \
  cargo test \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --features native \
  --test privileged_smoke \
  -- --ignored --nocapture
```

The test is ignored by default and requires the opt-in value to be exactly `1`.
It asserts actual raw events and a native user stack of at least two frames, not
just a successful attachment. Arrange capabilities according to the host's
security policy; an environment variable alone does not grant them. Build as an
ordinary user before running a test binary with any elevated privileges.

The capture example is:

```bash
cargo run \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --features native \
  --example capture -- \
  /absolute/path/to/tracer.ebpf.amd64 10 0 host
```

CPU IDs may be comma-separated, for example `0,1`. The final argument is `host`
(default) or `current`; the latter filters sampling to the current PID namespace.
The example writes newline-delimited owned JSON snapshots, including pending
windows and the final shutdown window. Set
`OTEL_ARROW_EBPF_CAPTURE_DIAGNOSTICS=1` for bounded error and lifecycle timing
details on stderr. `OTEL_ARROW_EBPF_EVENT_POLL_MS` can override the example's
polling interval for controlled comparisons; normal configuration validation
and the sampling/buffer safety clamp still apply.

The reproducible container smoke script requires a separate explicit opt-in:

```bash
OTEL_ARROW_EBPF_CONTAINER_TEST=1 \
  bash crates/upstream-ebpf-profiler-backend/tests/container_capture.sh \
  /absolute/path/to/capture \
  /absolute/path/to/workload \
  /absolute/path/to/tracer.ebpf.amd64 3 0,1
```

It always uses the current PID namespace, read-only mounts, a pinned image,
resource/time limits, no container network, and automatic cleanup. It requires
authorization for a privileged container and a tracefs mount inside it; it does
not join the host PID namespace or change host sysctls. Build the workload with
frame pointers as documented in [FEASIBILITY.md](FEASIBILITY.md).

An independent C ABI fixture can be run without BPF privileges when GCC or a
compatible C compiler and the pinned source tree are available:

```bash
UPSTREAM_EBPF_SOURCE=/absolute/path/to/opentelemetry-ebpf-profiler \
  cargo test \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --test c_abi_layout -- --ignored
```

It compiles only in a temporary directory and never writes into the reference.

## Supported behavior

When the native feature and required host permissions are available, the code
is designed to:

- load the native perf and probe stop/native targets, `native_tracer_entry`,
  and one kernel-version-specific `sched_process_free` program;
- populate and retain slots 0 and 1 in both `perf_progs` and `kprobe_progs`;
- attach CPU-clock perf events at a configurable frequency;
- consume `report_events` perf buffers and `trace_events` ring-buffer records;
- account for perf-buffer loss and the upstream ring-output failure metric;
- synchronize executable mappings from a configurable procfs root;
- translate page-aligned executable mappings correctly, including LLD layouts;
- retry incomplete metadata on later bounded refreshes when capacity or file
  availability recovers;
- protect process identity with PID start time;
- calculate upstream-compatible executable IDs;
- install frame-pointer commands through the upstream map-in-map ABI;
- update PID/page mapping entries;
- aggregate into bounded, deterministic, owned snapshots;
- detach and close owned resources through RAII and repeated shutdown calls.

These paths have constrained live evidence, not general production guarantees.
Generation attribution is bounded by observable procfs identity/mapping states;
the unchanged trace ABI has no kernel-side generation token. Mandatory cleanup
is not a hard real-time OS scheduling guarantee. `shutdown_incomplete` marks a deadline
that prevented proving the kernel buffers empty. Memory estimates are not a
physical-memory or RSS guarantee.

## Intentional exclusions

This milestone does not implement:

- OTAP, Arrow, OTLP, protobuf, or DFE receiver integration;
- symbolization;
- `.eh_frame`, `.debug_frame`, or Go `pclntab` extraction;
- managed-runtime unwind metadata or frames;
- Go custom labels;
- off-CPU, allocation, or memory profiling;
- OBI context correlation;
- custom kprobe/uprobe entrypoints;
- managed-runtime probe binding and custom probe attachment;
- full parity with the upstream Go userspace;
- artifact redistribution.

Several excluded features remain embedded in the upstream ELF. Their maps and
programs are inventoried in [COMPATIBILITY.md](COMPATIBILITY.md). Optional maps
are reduced to one entry where Aya permits resizing, but the object still
creates them because it is a monolithic artifact.

## Safety and ownership

The default crate forbids unsafe Rust. The `native` feature permits it only in
the small Linux OS-query module; each unsafe block states its invariant.
Dynamic inner maps use public, typed Aya APIs, not a custom BPF syscall shim.
The upstream ring intentionally disables wakeups, so draining uses a configurable
timer. Its default 10 ms interval is reduced when sampling rate and buffer/drain
limits require more frequent polling.

Kernel callback bytes are copied into bounded owned storage before decoding.
No borrowed ring-buffer memory reaches a snapshot. The process-global atomic
singleton is intentional: duplicate system-wide perf-event ownership in one
process would be incorrect; no sampled data is shared through that atomic.

## Further documentation

- [DEVELOPMENT.md](DEVELOPMENT.md): loader choice, build details, and commands.
- [FEASIBILITY.md](FEASIBILITY.md): evidence and all feasibility gates.
- [COMPATIBILITY.md](COMPATIBILITY.md): complete upstream object contract.
- [LICENSES.md](LICENSES.md): provenance and legal-review questions.
