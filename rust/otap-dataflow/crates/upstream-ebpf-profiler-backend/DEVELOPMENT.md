# Development Notes

## Pinned repositories

The investigation used these exact commits:

| Repository | Commit |
| --- | --- |
| `otel-arrow` starting point | `fa32548c6f0b5ffca8c414bd2c820f57d6102990` |
| OpenTelemetry eBPF Profiler | `06ea040c39d3d17bc1534a5dcc044368caf48782` |
| Lightswitch | `a9c1d0cc8c8c8b0eebdfa51e5b9b0ca43e358766` |
| Profile Bee | `b94ee599f0e03ee1e3273930a1bee9d2370da8bc` |
| Collector Releases | `1ce63a79ef35c4f858bfbdb3ae0025f071f3bdf4` |

All reference repositories were read-only. No source or artifact was copied
from them into this crate. The compatibility manifest contains facts obtained
from the external object but does not contain the object itself.

The experiment was created directly from the existing `origin/main` ref:

```text
branch: ebpf-profiler/02-upstream-kernel-rust-userspace
worktree: /home/labhas/personal/profiles-df-engine/otel-arrow-upstream-ebpf-spike
base: fa32548c6f0b5ffca8c414bd2c820f57d6102990
```

The investigation performed no fetch, branch switch in the independent worktree,
stash, or reset. The user subsequently authorized a commit and publication of
this separate branch to the `lalitb/otel-arrow` fork, without opening a PR or
updating `open-telemetry/otel-arrow`. This remains a developer-only experiment,
with no registered DFE component or changed collector behavior. It does not
advertise an unverified profiler in release notes.

## Upstream artifact construction

`support/ebpf/Makefile` builds every `*.ebpf.c` translation unit separately:

1. `clang-17` targets `x86_64-linux-gnu` or `aarch64-linux-gnu`.
2. It emits LLVM bitcode with `-O2`, `-g`, freestanding GNU C17, no standard
   includes, no jump tables, and deterministic source-prefix maps.
3. `llvm-link-17` links all translation units.
4. `llc-17 -march=bpf -mcpu=v2` emits one eBPF relocatable ELF.
5. `llvm-strip-17 --strip-debug --enable-deterministic-archives` removes
   ordinary debug sections while retaining `.BTF` and `.BTF.ext`.
6. Go architecture files embed either `tracer.ebpf.amd64` or
   `tracer.ebpf.arm64`.

`errors.h` is generated from `tools/errors-codegen/errors.json` by Go code.
The current object has `.text`, 38 executable program sections, `.maps`,
`.rodata`, `.rodata.var`, `license`, `.BTF`, and `.BTF.ext`.

## Loader evaluation

### Decision

Aya 0.14 is used for the optional implementation. `aya-obj` 0.3 is used for
non-privileged inspection.

libbpf-rs remains the preferred general-purpose C/eBPF loader, but it cannot
open this unmodified artifact. The object marks every macro-generated
`perf_*` and `kprobe_*` program symbol as local/static. The installed libbpf 1.6
runtime returned:

```text
sec 'kprobe/unwind_beam': program 'kprobe_unwind_beam' is static and not supported
```

A local entry symbol in a non-`.text` executable section causes this loader to
return `-ENOTSUP` before autoload selection. Consequently, its Rust wrapper
cannot reach pre-load program or map configuration for this artifact. The
failure was reproduced through `bpf_object__open_file`, with no BPF syscall.

The independent compile spike also found that fully vendored libbpf-rs 0.27
needs `autoreconf`; the host did not provide it. That packaging issue is
secondary to the reproducible ELF rejection.

The Aya compile spike is the actual feature-gated crate, not pseudocode:

```bash
cargo check \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --features native
```

Aya parsed both unmodified objects:

```text
amd64: programs=38 maps=47 btf=true btf_ext=true
arm64: programs=38 maps=47 btf=true btf_ext=true
```

### Feature matrix

| Requirement | libbpf-rs 0.27 | Aya 0.14 | Decision |
| --- | --- | --- | --- |
| Parse unmodified artifact | No: static program rejection | Yes | Aya |
| Enumerate before load | Normally yes, unreachable here | `aya-obj` yes | Aya |
| Set globals | Initial data APIs | `override_global` | Aya |
| Resize maps | `set_max_entries` | `map_max_entries` | Aya |
| Reuse map FD | `reuse_fd` | Pinned-map loading only | Not needed for perf path |
| Map-in-map templates | libbpf inner-map APIs | Parsed; creation compiled, not executed | Aya prototype |
| Dynamic inner maps | `MapHandle::create` | `Array::create`, `HashOfMaps::insert` | Aya |
| Program arrays | Map update APIs | `ProgramArray` | Aya |
| Perf-event attach | `attach_perf_event` | `PerfEvent::attach` | Aya |
| Ring buffer | `RingBuffer` | `RingBuf` | Aya |
| Perf buffer and loss callback | `PerfBuffer` | `PerfEventArray` | Aya |
| Verifier diagnostics | libbpf print/log APIs | `VerifierLogLevel` errors | Aya |
| Deterministic ownership | RAII | RAII | Both |
| Native probe map rewrite | Raw instruction API possible | Audited ELF REL rebinding before public loader APIs | Non-privileged equivalence proven |

### Native probe preparation

The upstream Go loader rewrites the explicit kprobe unwinder copies:

- tail calls from `perf_progs` to `kprobe_progs`;
- scratch state from `per_cpu_records` to `per_cpu_records_kp`.

Aya does not expose a high-level instruction-association setter on its loader.
The initial report treated this as a blocker. A subsequent isolated spike
proved that standard ELF relocation metadata can supply the same bindings
without private Aya APIs: two scratch-map references and one tail-array
reference change in each of `kprobe/unwind_native` and `kprobe/unwind_stop`.

`src/preparation.rs` now implements that fixed policy. It validates the original
fingerprint, compatible map ABIs, relocation lengths/opcodes/counts, and the
independently established prepared fingerprint. No executable instructions,
data, BTF, license section, or external file is changed. An opt-in test compares
Aya-linked instructions with a source-derived upstream FD-substitution oracle
for all 38 programs on both architectures.

This is source/relocation evidence, not live kernel evidence. Custom probe
entrypoints, managed-runtime probe preparation, and off-CPU profiling remain
excluded. Keeping the transformation pinned avoids accepting future upstream
layout changes implicitly.

## Native-only implementation choices

### Program set

The optional loader loads:

- `perf_unwind_stop`;
- `perf_unwind_native`;
- `native_tracer_entry`;
- `kprobe_unwind_stop`;
- `kprobe_unwind_native`;
- one of `tracepoint__sched_process_free` and
  `tracepoint__sched_process_free_pre616`.

It initializes indexes 0 and 1 in both program arrays. The perf-only minimum is
four programs; two additional native probe targets satisfy the upstream
dual-chain setup contract without attaching custom probes. It does not load interpreter,
off-CPU, generic-probe, integration-test, prctl, or system-analysis programs.

### Map strategy

Aya creates the monolithic object's maps. Required native maps receive explicit
bounded capacities. Maps embedded for excluded features are reduced to one
entry when their type permits resizing. All 16 outer stack-delta maps remain
because the native program contains a switch that can reference each bucket.

Dynamic inner stack-delta arrays use public Aya 0.14 APIs: `Array::create`,
`Array::set`, and `HashOfMaps::insert`. No custom BPF syscall layout is needed.
Closing a userspace inner-map FD after insertion leaves the outer map's kernel
reference. In contrast, both program-array userspace handles must stay alive to
prevent the kernel from clearing their tail-call slots.

Aya creates transient inner templates even for unused size buckets. The memory
estimate includes the largest 8,388,608-entry template with eight-byte element
stride, rather than counting only the two meaningful FP commands per executable.

### Native unwind milestone

The crate implements upstream-compatible:

- executable IDs: SHA-256 of first 4 KiB, last 4 KiB, and big-endian length;
- 64 KiB stack-delta pages;
- `StackDelta`, `StackDeltaPageInfo`, and outer map bucket selection;
- direct frame-pointer and invalid commands;
- PID/page LPM keys and 56-bit bias plus program-index values.

It does not claim general native unwind parity. It applies frame-pointer
commands over executable load segments, so the workload must preserve frame
pointers. Porting the upstream CFI extractor is a separate substantial body of
work.

### Architecture

amd64 is the implementation target. The arm64 artifact inventories
successfully, but native load requires an explicit inverse PAC mask and has not
been run. The object itself contains architecture-specific `pt_regs`, signal
frame, pointer-authentication, and Go runtime behavior.

### Kernel-stack discovery

Before loading the sampling runtime, a minimized temporary Aya object loads
`read_task_struct`. At most 64 reads of 128 task bytes locate a page-aligned
stack pointer whose syscall-entry register pointer lies within 64 KiB. One
additional request confirms the offsets. Only offsets are retained; kernel
addresses never enter logs or snapshots.

The temporary program is TID-filtered through the separately pinned one-byte
analysis patch documented in [LICENSES.md](LICENSES.md). Its namespace is always
the requesting thread's namespace, regardless of the sampler's PID mode.
Attaching only after a complete request and detaching before the next update
prevents request publication races. A live test exercises it while four other
threads issue syscalls on two CPUs.

The measured kernel layout is `task_struct.stack=32` and `stack pt_regs=16216`.
These values are discovered, not hardcoded for production. All temporary
analysis resources are released before the main map/program set is loaded.

### Generation and deadline contracts

Each observed PID/start-time/mapping state has an admission timestamp and a
verified-through timestamp in the kernel's monotonic clock. The backend
rechecks procfs after metadata upload and before attributing a copied batch.
Incomplete metadata retries preserve admission when the observed generation is
unchanged. Stale or unverifiable records receive explicit loss counters.

Local monotonic time is normalized using the bounded, signed time-namespace
offset. Mismatched current/child time namespaces at startup, or a namespace
change during capture, are rejected rather than guessed. An isolated live test
with a 1,000-second shift still collected recovered user stacks.

Shutdown disables ingress before optional draining. It performs no procfs
revalidation and admits only cached, already-verified time intervals. Budget
checks bound further perf/ring draining, decoding, aggregation, and finalization;
discarded owned records and aggregate multiplicities are counted. Mandatory
descriptor/storage release always runs. This is a bounded-work, deadline-aware
contract, not a hard real-time guarantee about kernel scheduling, storage, or
allocator latency.

## Packaging and default build

The crate's default features are empty. Rust-based `aya-obj` inspection is part
of the normal build. Aya and libc are Linux-only optional dependencies activated
by `--features native`.

No BPF object generation occurs in `build.rs`; there is no `build.rs`. The main
workspace therefore remains buildable without root, a BPF target, Clang, LLVM,
bpftool, libbpf, libelf development headers, or the upstream source tree.
The Criterion benchmark dependency brings in `alloca`, which uses a host C
compiler; it is not a normal backend dependency. The optional C ABI test uses
a host C compiler and external headers only when explicitly selected. Neither
case needs a BPF compiler.

Static/container deployment has not been demonstrated. Aya avoids a runtime
libbpf/libelf dependency, but a static target still needs its normal Rust/C
linker setup. A container must receive approved BPF/perf permissions and the
appropriate procfs, CPU topology, tracepoint, and BTF views. PID namespaces,
capabilities, kernel support, and GPL-artifact distribution are separate
deployment questions; a successful userspace build does not answer them.

## Unsafe-code audit

The default build forbids unsafe Rust. Native code otherwise denies it, with a
module-local allowance in `src/native/syscall.rs` for small OS queries:

- `sysconf(_SC_PAGESIZE)` has no pointer arguments.
- `gettid` identifies the exclusive analysis requester without pointer arguments.
- `clock_gettime(CLOCK_MONOTONIC)` writes one initialized local timespec.

Aya itself contains the lower-level map, mmap, perf, and BPF syscall unsafe
code. This crate does not expose Aya types in its snapshots.

## Validation commands

From `rust/otap-dataflow`:

```bash
cargo check -p otel-arrow-dfe-upstream-ebpf-profiler-backend
cargo check -p otel-arrow-dfe-upstream-ebpf-profiler-backend --features native

UPSTREAM_EBPF_OBJECT=/absolute/path/to/tracer.ebpf.amd64 \
  cargo test -p otel-arrow-dfe-upstream-ebpf-profiler-backend

UPSTREAM_EBPF_OBJECT=/absolute/path/to/tracer.ebpf.amd64 \
  cargo test \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --features native

cargo clippy \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --all-targets \
  --features native \
  -- -D warnings

UPSTREAM_EBPF_SOURCE=/absolute/path/to/opentelemetry-ebpf-profiler \
  cargo test \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --test c_abi_layout -- --ignored

UPSTREAM_EBPF_OBJECT=/absolute/path/to/tracer.ebpf.amd64 \
UPSTREAM_EBPF_ARM64_OBJECT=/absolute/path/to/tracer.ebpf.arm64 \
  cargo test \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --lib preparation::tests::external_probe_bindings_match_upstream_relocation \
  -- --ignored

cargo bench \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --bench event_decode --bench aggregation --bench reporting_window \
  -- --warm-up-time 0.1 --measurement-time 0.5 --sample-size 20 --noplot

cargo xtask check-benches
cargo xtask quick-check
cargo xtask check
```

The workspace currently emits a pre-existing Clippy configuration warning:
the disallowed-type entry for `once_cell::sync::Lazy` is not reachable. The
backend itself has no Clippy warning under `-D warnings`.

Normal tests explicitly ignore the privileged smoke test. When selected with
`--ignored`, it requires `OTEL_ARROW_EBPF_PRIVILEGED_TEST=1` and asserts a real
event and native stack. A skipped test is never evidence that a privileged
feasibility gate passed. Current results are recorded in
[FEASIBILITY.md](FEASIBILITY.md).

## Go reference comparison

The exact pinned Go source was built as a separate disposable oracle, not linked
to or spawned by the Rust backend. The public
`collector.BuildProfilesReceiver(WithReporterFactory(...))` extension point
accepts a direct reporter; it exposes processed traces without an OTLP listener
or network exporter. The oracle uses `interpreterconfig.NoInterpreters()`,
20 samples/second, sampling CPU 0, PID namespace translation, error frames
enabled, and an in-process metric reader.

The build uses Go 1.26.0, `CGO_ENABLED=0`, `osusergo,netgo`, and a local module
replacement pointing at the read-only pinned checkout. Keep the harness module
in its own directory: SDK and module/build caches must be outside that module's
package tree, or `go mod tidy` will incorrectly scan SDK test fixtures.
Do not run upstream `make`, `go generate`, or eBPF generation steps.

Go embeds the existing checked-in ELF; the produced oracle binary was checked
for an exact byte-for-byte occurrence of the pinned amd64 artifact. The
disposable build retains the harness, go.mod/go.sum, full module graph,
toolchain checksum, both licenses, dependency notices, and binary hashes.
None of those Go build artifacts is part of this Rust crate's runtime or
distribution.

The comparison runs each profiler separately in the same isolated measurement
image, with the same workload binary on CPU 0 and the profiler on CPU 1.
Go polls trace events at a fixed 250 ms; Rust was measured both at its 10 ms
default and with `OTEL_ARROW_EBPF_EVENT_POLL_MS=250`. Native CFI, map capacities,
preallocation, output aggregation, startup warmup, and final draining are not
equivalent and are explicitly reported in [FEASIBILITY.md](FEASIBILITY.md).

## Reference implementation notes

Lightswitch uses libbpf-rs, generated skeletons, separate capability probes,
explicit perf syscalls, and dynamic map-in-map creation. It is useful evidence
for a Rust/libbpf deployment, but its own object is libbpf-compatible.

Profile Bee uses Aya, bounded map shards, frame-pointer and DWARF paths, and
explicit process lifecycle tracking. It is useful evidence that Aya can support
this class of profiler, but this crate does not import its implementation.

The official Collector distribution packages the Go receiver for Linux amd64
and arm64. That confirms the upstream product boundary but does not provide a
native Rust userspace component or a reusable object-distribution contract.
