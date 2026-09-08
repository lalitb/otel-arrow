# Native Rust eBPF profiler

`otel-arrow-dfe-ebpf-profiler` is an incubating, independently testable,
runtime-neutral native CPU profiling library. It loads one Linux eBPF program,
attaches perf events to selected CPUs, drains per-CPU perf buffers through
disjoint shards, aggregates samples under explicit limits, and transfers owned
`ProfileSnapshot` values to a bounded synchronous consumer.

The crate does not depend on the DF engine, OTAP, Arrow, OTLP, protobuf, gRPC,
the Go runtime, FFI, or Tokio.

## Supported first milestone

- Linux x86_64 and aarch64 backend; privileged compatibility must be exercised
  on each target kernel before deployment.
- Host-wide on-CPU sampling at a configured frequency.
- Native user instruction addresses using the kernel stack helper. Frame
  pointers are the guaranteed deployment mode.
- Optional kernel instruction addresses.
- Single, per-NUMA, and fixed-count shard planning.
- PID, TID, process name, thread name, executable path, and executable mappings.
- Mapping-relative address normalization and bounded ELF symbol lookup.
- Owned, graph-validated, transport-neutral snapshots.
- Bounded queues, tables, file reads, metadata strings, and error details.
- Separate kernel output failures, perf lost notifications, userspace sample
  drops, optional metadata omissions, and shutdown deadline counters.
- Process-local singleton ownership, startup rollback, and idempotent
  deadline-aware shutdown.

Not supported in this milestone: per-core workers, off-CPU or allocation
profiling, arbitrary native unwinding without frame pointers, `.eh_frame`,
DWARF, inline-frame reconstruction, managed-runtime unwinding, executable
upload, trace correlation, OTLP/OTAP output, or DF-engine receiver integration.

## Build

Ordinary workspace checks need no eBPF compiler or privileges:

```bash
cd rust/otap-dataflow
cargo check -p otel-arrow-dfe-ebpf-profiler
cargo test -p otel-arrow-dfe-ebpf-profiler
```

Build the separate GPL-2.0-only kernel object with Clang's BPF backend:

```bash
./rust/otap-dataflow/ebpf/profiler/build.sh
```

The object is generated under `rust/otap-dataflow/ebpf/profiler/target/` and is
not checked into Git. The build also emits `<object>.manifest` with ABI,
architecture, source/object digests, compiler, program/map names, and license.
The loader validates this bounded manifest before Aya parses the object.
Configure the object path explicitly or set `OTEL_EBPF_PROFILER_OBJECT`.

## Capture example

```bash
cargo run -p otel-arrow-dfe-ebpf-profiler --example capture -- \
  --duration 10s \
  --reporting-interval 2s \
  --samples-per-second 19 \
  --sharding per-numa \
  --object ebpf/profiler/target/profiler.bpf.o
```

The example prints bounded summary information rather than host stacks.

## Resource model

`Limits` bounds host-influenced state. Static validation uses checked arithmetic
to account for table/index storage, metadata strings, symbol/procfs reads,
thread stacks, per-CPU buffers and maps, frozen windows, merge scratch, and
completed snapshots. Snapshot byte limits are enforced during admission, not
only after a potentially oversized window has been built.

Defaults include 128 monitored CPUs, 8 maximum shards, 64 KiB perf data per CPU,
128 events per shard batch, 256 processes, 1,024 threads, 2,048 mappings, 2,048
user stacks, 512 kernel stacks, and 64 frames per stack. Each output is limited
to 4 MiB; two frozen windows and two completed snapshots may wait for consumers.
The configured memory envelope must fit 256 MiB. Actual NUMA shard count must
fit the frozen-window slots; raise the slots (and satisfy the recalculated
memory envelope) on hosts with more than two NUMA nodes.

The current default peak envelope is about 238 MiB, including the larger
initialization phase. Aya's eager kernel-BTF read is preflighted against explicit
byte/type/member caps, and verifier retry allocation is included. The generated
object limit is 256 KiB. These are deployment bounds, not a measured RSS promise.

Inspect the complete allocation breakdown without loading eBPF:

```bash
cargo run -p otel-arrow-dfe-ebpf-profiler --example capture -- \
  --memory-estimate-only
```

This is a requested-storage envelope, not a guarantee about allocator
fragmentation or process RSS. Snapshots retained by consumers after ownership
transfer are the consumer's responsibility. Injected test/embedding providers
must honor the documented bounded input and callback contracts.

The kernel side has bounded CPU-indexed maps and one 1,072-byte scratch event
per possible CPU. A shared permit budget covers frozen windows in both the
channel and the finalizer, preventing duplicated queue accounting.
Slow consumers cause counted sample loss rather than blocking kernel draining.
Kernel output failures can overlap perf lost notifications; do not add them.

## Privileged smoke test

The ordinary test suite skips the host-wide test. After building the object and
granting suitable BPF/perf privileges:

```bash
OTEL_EBPF_PROFILER_SMOKE=1 \
OTEL_EBPF_PROFILER_OBJECT=ebpf/profiler/target/profiler.bpf.o \
cargo test -p otel-arrow-dfe-ebpf-profiler --test real_kernel -- --nocapture
```

The test requires Linux, a supported architecture, and the effective BPF/perf
capabilities required by the running kernel (normally `CAP_BPF` plus
`CAP_PERFMON`, or legacy `CAP_SYS_ADMIN`). UID 0 alone is not sufficient in a
capability-restricted container. Lockdown and kernel policy can still deny
attachment. A prerequisite skip is not a successful real-kernel capture.

## Ownership and shutdown

`ProfilerBuilder::start` returns a running `Profiler` and a bounded
`SnapshotReceiver`. Take snapshots with `recv_timeout` or `try_recv`; no clone
of live caches is required. `Profiler::cancel` stops ingress without waiting.
`shutdown(deadline)` drains, finalizes, and joins, returning a `ShutdownReport`.

A wait timeout retains worker, map, and singleton ownership for retry. Samples
drained after the deadline are counted as shutdown loss without expensive
metadata work. Dropping the handle is a safety net that joins workers; it does
not detach live threads after an arbitrary one-second wait. Providers supplied
through dependency injection must therefore obey their bounded-call contract.
Runtime failure is visible through `Profiler::failure`, the receiver, and the
shutdown report independently of whether resource cleanup succeeds.

## Limitations

The first milestone uses startup CPU/NUMA topology; CPU hotplug requires restart.
PID and TID generations are checked separately from periodic mapping refresh.
Exec/mmap events and historical mapping generations are not yet tracked, so
mapping changes have refresh latency. Use the host PID namespace or a compatible
host procfs mount; automatic PID/time-namespace translation is not implemented.
Stripped, deleted, inaccessible, oversized, or unsupported object files retain
addresses and fixed diagnostics rather than fabricated symbols.

## Future extraction

The intended standalone repository can separate `profiler-core`,
`profiler-linux`, and `profiler-ebpf` crates while preserving the public
snapshot and configuration model. During incubation, consumers should pin an
exact `otel-arrow` Git revision. A later branch will add a thin DF-engine
receiver that consumes `ProfileSnapshot` by ownership and builds OTAP Profiles
Arrow data directly.

See [DEVELOPMENT.md](DEVELOPMENT.md) for architecture and decision records and
[REFERENCE.md](REFERENCE.md) for reference-profiler findings.
