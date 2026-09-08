# Native profiler development

## Architecture

The process-wide coordinator validates static configuration, acquires a
singleton lease, discovers topology, plans exact global quotas, prepares one
kernel object and per-CPU buffers, then starts one worker per shard. Each worker
establishes affinity (if requested) and initializes bounded state before the
coordinator enables sampling perf events. Every acquired stage has an owner
that participates in rollback.

Workers own disjoint CPU buffers and aggregate without a global hot-path lock.
At each reporting boundary they transfer a frozen aggregation window through a
bounded channel. A shared permit pool covers both queued and pending windows;
the count is never multiplied by the number of reporting generations.
A single finalizer sorts and compacts the window, remaps snapshot-local indexes,
validates semantic graph ownership and accounting, deduplicates tables, and
reaggregates equal samples. CPU is an explicit sample dimension. Unmapped
addresses and empty/error stacks retain process and stack-kind identity.
Queue saturation causes counted sample-window loss, not unbounded memory or
blocked kernel draining.

Shutdown detaches ingress first, then drains buffered records. Once its deadline
passes, drain workers count discarded samples without doing further metadata or
symbol work. Finalization and joins can finish on a later call after
`ShutdownTimeout`; the lease and resources remain owned in the meantime.
The destructor joins instead of detaching workers. A worker/source failure is
reported, stops ingress, and initiates cleanup rather than producing apparently
healthy empty windows.

The native backend and injected providers must honor bounded call contracts.
Synchronous OS/filesystem stalls cannot be forcibly cancelled safely by Rust.
Explicit deadlines bound caller waiting, while destruction prioritizes joining
over silently leaking a callback that violates that contract.

## Aya and kernel-language decision

The user-space implementation uses Aya 0.14.0. The kernel program is original C
compiled separately with Clang's BPF backend.

Aya was selected because it keeps the deployed user-space binary independent of
libbpf, libelf, bpftool, generated skeletons, and a mandatory C runtime. It
supports perf-event programs, per-CPU perf buffers, BTF loading, x86_64, and
aarch64. The initial sampler uses only stable helpers and fixed types, so it
needs neither kernel type headers nor CO-RE relocations. This avoids a known
limitation of a pure Rust Aya eBPF build: Rust-generated BTF does not by itself
provide Clang-style `.BTF.ext` CO-RE relocation records.

`libbpf-rs` remains a credible fallback for future kernel-structure access or
advanced unwind machinery. Its mature CO-RE and skeleton workflow comes with
Clang/LLVM, libbpf, libelf, and generated-source or object provenance
requirements. Those costs are not justified for the helper-only first
milestone.

The C kernel source was chosen over an Aya Rust `no_std` crate because the
repository's pinned stable Rust toolchain cannot compile the BPF target without
an additional nightly/bpf-linker toolchain. The separate C command is easier to
reproduce and does not affect ordinary Cargo workspace builds.

## ABI and transport

ABI v1 is a fixed 1,072-byte little-endian event:

- 48-byte little-endian fixed-width header;
- PID, TID, CPU, BOOTTIME timestamp, flags, depths, and helper errors;
- 64 user instruction addresses;
- 64 kernel instruction addresses.

The fixed layout avoids discriminant or alignment ambiguity and permits strict
checked decoding. Perf transport framing is a separate layer: it accounts for
alignment padding and wrapping before passing the exact ABI payload to the
decoder. The program uses per-CPU scratch because the event exceeds the
512-byte BPF stack. Unused slots must not contain stale frames.

Per-CPU perf buffers were selected for disjoint CPU-to-worker ownership and
observable loss, not an unmeasured speed advantage over ring buffers.
BOOTTIME timestamps require a sufficiently recent kernel regardless of
transport. A global ring buffer is a future measured alternative, not a
benchmark loser.

## Memory and capacity

All table, queue, file-read, string, thread, and snapshot capacities are explicit.
`ProfilerConfig::memory_estimate` uses checked arithmetic and covers:

- configured per-CPU perf buffers;
- one decoded-event scratch batch per maximum shard;
- bounded process, thread, mapping, function, location, stack, and sample rows
  plus their retained indexes and duplicated key strings;
- explicit thread stack reservations and fixed bookkeeping;
- active finalization tables, index remaps, and output;
- a shared permit budget for all pending shard windows;
- pending completed snapshots;
- one bounded procfs read per shard;
- one bounded symbol-file read per shard;
- power-of-two hash-table and vector-capacity allowance for the pinned
  implementation, rather than an unexplained percentage margin.

The calculation considers the largest possible single-shard window when
budgeting pending slots. Dividing by the configured maximum shard count would
underestimate Single mode. It is a requested-storage rejection boundary, not an
RSS guarantee; allocator fragmentation, mapped code, and consumer-owned copies
are outside the allocation contract. Kernel possible-CPU state is accounted
separately from the smaller selected-CPU set.

Global table capacities are partitioned into exact per-shard quotas whose sum
does not exceed the configured limit. Pending shard capacity counts individual
snapshot values, not reporting generations.

Aya 0.14 eagerly reads kernel BTF during loader construction, even before
`.btf(None)` is applied. The Linux backend therefore preflights the immutable
sysfs BTF export with explicit byte/type/member caps. Initialization reserves
its parser allocations and a 32 MiB verifier-retry allowance separately from
running workers. The kernel contract tests constrain the actual dependency's
type/member layouts used by the estimate. See the kernel boundary README for
the precise assumptions; no private Aya fork or filesystem redirection is used.

When a capacity is reached, the profiler retains existing data, rejects new
cardinality, increments a stable counter, and continues draining.

Sample-drop reasons count samples, including the full aggregate count of a
dropped window. Optional metadata omissions do not count as sample loss.
Kernel helper output failures and perf lost notifications can overlap and must
not be added together. Shutdown timeouts count calls separately from actual
discarded samples.

## Metadata and symbols

PID/start-ticks and TID/start-ticks form generation identities. Lightweight
identity checks are independent of the one-second executable-mapping refresh.
Snapshot frames never reference a live cache. Mapping and function identity
include their owning process/mapping, so equal virtual addresses or symbol
names in unrelated processes cannot merge.

Device/inode identity is retained in each mapping and passed to native symbol
lookup. The descriptor read for a new symbol index must match that identity
before and after reading; a replaced pathname cannot silently supply another
object's symbols. Procfs start-time identities have clock-tick precision, not
nanosecond precision.

ELF lookup consumes file-relative mapping addresses and translates them using
object layout, not a direct comparison with ELF virtual addresses. File indexes,
names, object reads, negative lookups, and eviction are bounded. Stripped or
unavailable objects preserve addresses without inventing a function.

CPU hotplug reconciliation, exec/mmap event ordering, historical mappings,
managed runtimes, DWARF and inline frames remain explicit limitations.

## Licensing and provenance

- User-space Rust source: Apache-2.0, under the repository license.
- Kernel C source: GPL-2.0-only, declared in the source SPDX header and BPF
  license section.
- Kernel object: generated locally from the kernel C source and not checked in.
- Object manifest: generated with ABI, architecture, source/object digests,
  compiler, program/map names, and license; validated before Aya loads the ELF.
- Reference profiler source, constants, fixtures, coredumps, generated objects,
  and unwind data are not copied.

The split avoids distributing an unexplained binary artifact. Packaging a
generated GPL kernel object with an Apache user-space binary should receive
normal project legal review before release; this document does not make a legal
compatibility conclusion.

Manifest digests detect corruption and mismatched artifacts; they are not a
signature or proof of trusted authorship. Object deployment remains a trusted
administrative operation.

## Changelog decision

This branch adds an incubating internal library and no registered DF-engine
component, configuration surface, or released end-user behavior. Under the
repository's user-facing changelog rules it does not add a `.chloggen` entry.
The later receiver branch will require its own component changelog entry.

## Advanced unwinding seam

Future `.eh_frame` or compact unwind support should be a bounded implementation
behind the event-source/platform boundary. It needs explicit per-process unwind
bytes, map entries, eviction policy, architecture-specific validation, and loss
counters before it can supplement the guaranteed frame-pointer mode. DWARF,
inline frames, Go, JVM, Python, V8, .NET, Ruby, PHP, Perl, and Erlang remain
separate milestones.

## Extraction plan

An extracted repository can use:

```text
rust-ebpf-profiler/
|-- crates/
|   |-- profiler-core/
|   |-- profiler-linux/
|   `-- profiler-ebpf/
|-- examples/
|-- benches/
`-- tests/
```

`ProfilerConfig`, `ProfilerBuilder`, `ProfileSnapshot`, statistics, and typed
errors form the initial API boundary. Extraction should establish semantic
versioning, compatibility byte fixtures, signed/reproducible kernel-object
releases, cross-platform stubs, license packaging, and exact revision pinning
for incubating consumers.

## Benchmarks and comparison

### Real-kernel milestone evidence

The opt-in smoke completed without a prerequisite skip on signed Ubuntu
`5.15.0-191-generic` (`5.15.209`) under QEMU `10.2.1` TCG. It loaded and attached
the user-only program, observed the pinned frame-pointer child's user stack,
validated the owned graph and counters, shut down idempotently, and checked FD
cleanup. A second startup loaded the user-plus-kernel program and produced
another valid real-event snapshot before cleanup.

The guest used one virtual CPU and 768 MiB RAM, with no network, disks, host
filesystem sharing, or host privilege changes. This establishes the x86_64
real-kernel path; it is not an aarch64 runtime, multi-CPU hardware, host-kernel,
or comparative performance measurement. Both architecture objects also passed
repeated-build, original-C, ABI, and selected-Aya parser/layout contracts.

The complete workspace suite passed with `RUST_TEST_THREADS=4`. A default-thread
run hit the previously observed unrelated OTAP exporter
`test_shutdown_nacks_correlated_pdata` race; no unrelated source was changed.

### Focused measurements

The three Criterion targets cover fixed-ABI decoding, user/kernel stack
interning, repeated aggregation, actual new-key insertion, explicit capacity
rejection, mapping lookup, native symbol-cache lookup, finalization, ownership
handoff, reset, and slow-consumer queue pressure. Every new-key batch starts
empty and asserts admission; it cannot accidentally measure a full-table
rejection path as insertion speed.

The earlier high-cardinality measurements did exactly that and are not a valid
insertion baseline. Measurements must be rerun after correctness changes.

The corrected Criterion run on the local x86_64 WSL2 host measured:

| Operation | Interval |
| --- | --- |
| Checked 32-frame event decode | 169.74-174.61 ns |
| Repeated user stack | 712.13-727.51 ns |
| Repeated user plus kernel stack | 1.1583-1.2005 us |
| 512 admitted distinct stacks | 1.1195-1.1491 ms |
| Full thread-table rejection | 130.91-133.02 ns |
| Mapping lookup among 512 ranges | 12.338-12.655 ns |
| Native symbol-cache lookup with file identity refresh | 4.4789-4.6520 us |
| Finalize 1,000 sample rows | 23.292-24.508 us |
| Snapshot ownership handoff | 298.45-348.50 ns |
| Empty-window initialization/reset | 226.17-232.45 ns |
| Full consumer queue rejection including snapshot destruction | 17.399-18.091 us |

These are microbenchmarks, not end-to-end profiler overhead. Input validation,
snapshot shape, and insertion workloads changed during hardening, so the old
pre-hardening values are not controlled before/after comparisons. No Go or
Lightswitch runtime superiority is claimed.

The default running envelope after integration is 244,069,824 bytes.
Initialization is bounded separately at 249,815,040 bytes (about 238 MiB).
These phases do not overlap; their maximum is below the configured 256 MiB
ceiling. The running categories are
16,777,216 perf-buffer bytes; 5,423,104 kernel-map bytes; 11,116,544 worker bytes;
23,428,416 active-aggregation bytes; 46,856,832 pending-window bytes; 35,852,544
finalization bytes; 16,788,736 completed-snapshot bytes; and 87,826,432 scratch
bytes. Use `capture --memory-estimate-only` for the authoritative calculation
after further layout or configuration changes.

`tools/compare.sh --plan-only` prints the comparison command arrays without
profiling. With `--acknowledge-host-profiling`, the harness executes prebuilt
native Single/PerNuma/Fixed and slow-consumer cases. `--build` builds once before
timing. Go, Go-plus-DFE, and Lightswitch baselines are explicit JSON argv arrays;
no shell text is evaluated.

The harness records process/steady CPU, peak/steady RSS, context switches,
observed migrations, NUMA page placement, normalized native profiler metrics,
and shutdown time. Missing baseline metrics remain unavailable, not zero.
NUMA page placement is not a measurement of local/remote memory accesses;
model-specific performance counters are needed for those. The harness tests
run ordinary subprocesses, not privileged profiling workloads.
