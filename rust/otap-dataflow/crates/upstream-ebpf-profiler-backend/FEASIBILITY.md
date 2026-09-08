# Feasibility Report

## Decision

**Decision: feasible for native frame-pointer workloads on the tested Linux
amd64 kernel, with an explicitly pinned, analysis-only request-filter patch.**
The preferred libbpf-rs loader cannot open the unmodified artifact. The Aya
alternative now loads the native programs, initializes both tail-call chains,
attaches perf events, supplies metadata, and produces real owned snapshots
without Go in the runtime path.

This is not a conclusion that a Rust userspace implementation is impossible.
It is a precise statement about the evidence available in this run:

- libbpf and libbpf-rs reject the unmodified object before inspection because
  its section entry symbols are local/static;
- Aya parses the artifact and exposes the required perf-path primitives;
- the feature-gated Aya path collected native stacks up to 41 frames;
- isolated one-CPU and two-CPU captures passed, with no recorded buffer,
  capacity, or metadata drops after fixing overlayfs handling;
- real startup-failure injection and repeated shutdown restored descriptor
  counts at each tested phase.

The loader-specific decision is **not feasible with the selected artifact and
libbpf-rs**, followed by the required alternative-loader evaluation. This is
not a negative verdict on Rust userspace or on the Aya path.

Kernel-mode register recovery, conservative PID/mapping-generation attribution,
bounded deadline-aware drain/finalization, and the Go comparison now have
working evidence. This is still an experimental native-FP backend, not managed
runtime or full-CFI parity, an all-kernel guarantee, or a hard real-time system.
Keep it separate from the independent profiler until deployment, maintenance,
and distribution decisions are made.

## Environment evidence

The experiment ran on:

```text
Linux 6.6.114.1-microsoft-standard-WSL2 x86_64
uid=1000
CapEff=0000000000000000
unprivileged_bpf_disabled=2
memlock soft limit=65536
/sys/kernel/btf/vmlinux readable, 6138563 bytes
```

The host had a libbpf 1.6 runtime and `libelf.so.1`, but no `bpftool`,
LLVM BPF tools, libbpf development headers, libelf development headers, or
`autoreconf`.

The privileged capture example stopped before a BPF syscall with:

```text
Unsupported("effective capabilities 0x0 lack CAP_BPF+CAP_PERFMON or CAP_SYS_ADMIN")
```

On 2026-09-05 the user authorized short non-interactive privileged smoke tests.
`sudo -n id` returned:

```text
sudo: interactive authentication is required
```

An explicitly opted-in smoke-test invocation also stopped at the capability
guard above. No BPF load, attachment, or verifier execution occurred. No password
was requested, and no sysctl, mount, capability, or reference file was changed.
The non-interactive privilege check was repeated on 2026-09-06 with the same
authentication blocker.

The user subsequently authorized a separate route: temporary privileged Docker
containers with read-only experiment/artifact mounts, a read-only tracefs mount
inside the container when needed, automatic cleanup, and capture restricted to
the container PID namespace. This route succeeded. No host sysctl or persistent
capability setting was changed; no host PID namespace was joined.

The local Docker Desktop endpoint is `unix:///var/run/docker.sock`. The base
image is pinned to
`ubuntu@sha256:2260313b31c8c011cd2eebe728008efac1b3982be73eb71348ea2648d2c0e09b`.
It has glibc 2.43 and the same Linux 6.6.114.1 WSL2 kernel. Test containers use
`--rm --init --network none --privileged --read-only`, a 512 MiB memory limit,
64-process limit, and explicit CPU affinity.

Namespace isolation is based on upstream `get_pid_tgid`: tasks outside the
configured namespace return before `collect_trace`. Upstream intentionally
disables the fast `sched_process_free` path in this mode; bounded procfs sweeps
provide exit detection. Snapshot PIDs and executables belonged to the temporary
test container. Every test container was removed after its run.

## Evidence gathered

### Artifact opening

`aya-obj` 0.3 opened both unmodified artifacts without root:

```text
amd64: programs=38 maps=47 btf=true btf_ext=true
arm64: programs=38 maps=47 btf=true btf_ext=true
```

The checked-in inventories have these JSON SHA-256 values:

```text
amd64: 35d5a79c4615603612d66e1f26493e4debc092289785ed5aa2ebfde6219785e5
arm64: a5ee89a6051513ad8d512b8c64681d5cdf8f0eb0b3a8b1997a921c66ae8c3941
```

Both external artifacts pass `inspect ... --check`, including the complete
inventory and pinned commit. Unsupported manifest schemas and changed tail-call
destinations are rejected before any kernel operation.

### Independent ABI evidence

`tests/c_abi_layout.c` was compiled with GCC 15.2 against the external pinned
`types.h`, `kernel.h`, and `errors.h`. Its executable lived only in a temporary
directory and was deleted after the test. It independently confirmed:

```text
Trace size / frame_data offset: 25304 / 728
frame_data_len / num_frames / num_kernel_frames: 700 / 702 / 704
origin / value / cpu_id: 706 / 712 / 720
StackDelta / StackDeltaPageKey / StackDeltaPageInfo: 4 / 16 / 8
PIDPage / PIDPageMappingInfo: 16 / 16
```

### libbpf-rs blocker

A libbpf-rs 0.27 compile spike first hit a missing `autoreconf` dependency for
its fully vendored build. The installed libbpf 1.6 was then called directly
to separate packaging from object compatibility. It returned:

```text
libbpf: sec 'kprobe/unwind_beam': program 'kprobe_unwind_beam' is static and not supported
open failed: -95
```

The direct opening failure was reproduced on 2026-09-05. It is independent of
the missing build dependencies: installing them cannot make this installed
libbpf accept the object. No artifact or loader patch was applied.

### Aya implementation evidence

Aya 0.14 provides:

- `override_global`;
- `map_max_entries`;
- BTF map-in-map creation;
- `ProgramArray`;
- `PerfEvent::attach` with CPU clock frequency;
- `PerfEventArray` with explicit lost records;
- `RingBuf`;
- verifier-log controls;
- RAII-owned programs, maps, links, buffers, and perf events.

The crate's native feature compiles and its non-privileged tests pass. Dynamic
inner maps use the public `Array::create`, `Array::set`, and
`HashOfMaps::insert` APIs. The earlier custom BPF syscall shim was removed.

The loader retains the program-array handle for the entire session: closing
its last userspace reference would clear tail-call slots even while programs
still referenced the map. Event buffers are prepared before lifecycle hooks
and sampling attach. These are source/ownership facts, not live kernel results.

## Smallest native-only dependency graph

```text
native_tracer_entry
  -> .rodata.var globals
  -> per_cpu_records
  -> metrics
  -> pid_page_to_mapping_info
  -> go_procs (unconditional lookup, labels remain disabled)
  -> perf_progs[0] = perf_unwind_stop
  -> perf_progs[1] = perf_unwind_native

perf_unwind_native
  -> per_cpu_records
  -> metrics
  -> pid_page_to_mapping_info
  -> stack_delta_page_to_info
  -> exe_id_to_{8..23}_stack_deltas
  -> unwind_info_array
  -> interpreter_offsets
  -> perf_progs

perf_unwind_stop
  -> per_cpu_records
  -> metrics
  -> apm_int_procs
  -> traces_ctx_v1
  -> go_procs
  -> trace_events
  -> pid_events
  -> reported_pids
  -> report_events
  -> inhibit_events
  -> perf_progs

sched_process_free
  -> pid_page_to_mapping_info
  -> reported_pids
  -> pid_events
  -> report_events
  -> inhibit_events
  -> metrics
```

Userspace must additionally provide:

```text
/proc/<pid>/stat
  -> PID start-time identity

/proc/<pid>/maps and map_files
  -> executable mappings
  -> ELF virtual-address translation
  -> executable ID
  -> frame-pointer stack-delta plan
  -> dynamic inner array
  -> outer stack-delta map
  -> stack_delta_page_to_info
  -> pid_page_to_mapping_info

trace_events
  -> checked ABI decoder
  -> bounded queue
  -> process identity
  -> bounded aggregator
  -> owned snapshot
```

The loader also initializes the two native probe targets. Their shared-map
dependencies match the native perf chain, but they use `per_cpu_records_kp`
and `kprobe_progs[0,1]` after checked preparation. No custom probe is attached.

## Native probe-map equivalence

An isolated Rust spike established that six standard ELF64 REL symbol
substitutions reproduce the upstream Go userspace's native probe-map binding.
That policy is now implemented in `src/preparation.rs`, using only public
object-reading and Aya APIs. It changes metadata in an owned copy, not upstream
files or executable instructions.

For both pinned architectures, a non-privileged test performs Aya map relocation
with synthetic descriptors and links function calls. It compares all 38 resulting
programs against a source-derived FD-substitution oracle. Only the two native
probe programs have different bindings; every other program and every
non-target ELF section remains unchanged.

Original and prepared fingerprints are both checked. Exact prepared hashes and
licensing/provenance boundaries are in [COMPATIBILITY.md](COMPATIBILITY.md) and
[LICENSES.md](LICENSES.md). This removes the earlier loader-API blocker for the
native probe subset; it does not demonstrate verifier acceptance.

## Feasibility gates

| Gate | Result | Evidence or remaining blocker |
| --- | --- | --- |
| 1. Open and inspect unmodified artifact | Pass with Aya; fail with libbpf | Both amd64 and arm64 inventories produced without root |
| 2. Configure every required global | Pass for the supported native path | All globals configured; task-stack/register offsets discovered dynamically; unused interpreter/VMA/age fields disabled |
| 3. Resize required maps before load | Pass on tested kernel | Live map creation honored configured capacities; fdinfo/map inspection recorded them |
| 4. Reproduce upstream map rewrite | Pass for native subset, non-privileged | Six REL bindings match the source-derived oracle across all 38 linked programs on both architectures |
| 5. Create and populate map-in-map | Pass on tested kernel | Four dynamic unwind arrays were observed under owned outer maps; real stacks used them |
| 6. Initialize perf and probe arrays | Pass for setup | Both native arrays populated successfully; perf tail calls produced deep stacks |
| 7. Isolate minimum native set | Pass for tested setup | Six selected native/lifecycle programs loaded; no managed/custom probe programs attached |
| 8. Attach and run one perf entry | Pass | One-CPU and two-CPU namespace-filtered captures produced real events |
| 9. Decode a real upstream event | Pass | Owned snapshots contain real workload stacks up to 41 native frames |
| 10. Supply process metadata without Go | Pass for the declared observation contract | Metadata epochs, PID reuse/mapping-change rejection, bounded verification, and shifted-time-namespace capture are exercised |
| 11. Populate native unwind metadata | Pass for FP subset | Frame-pointer maps produce meaningful native stacks; full CFI remains excluded |
| 12. Bound host-influenced userspace state | Bounded logical contract | Counts/bytes/work units, kernel-BTF inputs, and retry policy are bounded; physical allocator/OS timing is not claimed as a hard guarantee |
| 13. Bound and estimate BPF map memory | Measured tested configuration | 51 owned maps including four children used about 109.37 MB; worst-case startup peak still needs qualification |
| 14. Deterministic rollback and shutdown | Pass for tested bounded-work contract | Eight startup fault points restore descriptors; zero/repeated shutdown and exact expired-backlog accounting pass; mandatory cleanup is not real-time |
| 15. Document licensing boundary | Engineering record only | Artifact remains external; distribution/legal approval is still required |
| 16. Maintain pinned compatibility | Pass | Commit, digest, full ABI inventory, and deterministic manifest are checked in |

## Native runtime caveats

### Kernel-mode interruption

The upstream entrypoint can recover user registers from kernel entry state by
using `task_stack_offset` and `stack_ptregs_offset`. Rust now derives them with
the existing `read_task_struct` program: at most 64 bounded task reads locate
the stack field, and one additional read confirms the layout. The measured
offsets were 32 and 16,216 bytes, respectively.

The original shared request is TGID-filtered and can race concurrent threads.
A separate analysis-only policy changes one checked instruction byte to read
the namespace TID instead. Requests use the caller's local TID and its own PID
namespace, independently of the sampler's PID mode. Exact bytes and hashes are
documented in [LICENSES.md](LICENSES.md). Temporary analysis resources are
released before normal sampling begins; kernel addresses are never exposed.

In a syscall-heavy workload, the previous loader produced 48 register-read
failures in 59 samples. Dynamic discovery removed that failure: 53 of 57 samples
contained 36 native frames. A concurrent test repeated discovery while four
other threads issued syscalls on two CPUs, without corrupting requests.

### VMA helper

The Go loader rewrites unsupported `bpf_get_current_task_btf` and
`bpf_find_vma` calls out of every program. Aya does not expose this rewrite.
The spike requires Linux 5.17 or newer and disables the runtime branch with
`vma_lookup_enabled=0`. This avoids the known old-kernel verifier blocker but
does not reproduce the upstream compatibility range.

### Probe unwinders

The source emits explicit perf and kprobe copies, but both copies initially
refer to `perf_progs` and `per_cpu_records`. The Go loader rewrites kprobe
copies to `kprobe_progs` and `per_cpu_records_kp`. The Rust loader now reproduces
these bindings for the native stop/native pair through audited preparation.
Managed-runtime probe bindings and custom probe attachment remain excluded.
Concurrent custom-probe execution still needs separate live proof.

Both chain setups now pass kernel loading. The perf chain is exercised by live
capture; no custom kprobe/uprobe attachment has been added solely to exercise
the probe chain.

### Monolithic maps

Disabling managed-runtime programs does not remove every managed-runtime map
from the ELF. Aya creates the maps before individual programs are loaded. The
spike reduces unused maps to one entry, but their ABI remains part of the
compatibility contract.

### Kernel stack capture

`collect_trace` always calls `push_kernel_frames`; the artifact has no global
to disable it. `include_kernel_stacks=false` discards those addresses in Rust,
but does not avoid kernel work or ring-buffer bytes.

## Process synchronization findings

The upstream Go userspace:

- receives PID/TID notifications through a perf-event array;
- drains `pid_events`;
- tracks recent reports through `reported_pids`;
- attaches `sched_process_free`;
- optionally watches `prctl(PR_SET_VMA_ANON_NAME)`;
- periodically cleans live PIDs;
- delays exit cleanup until earlier trace timestamps are processed;
- handles main-thread exit and PID reuse races;
- opens deleted files through `/proc/<pid>/map_files`;
- derives a stable 128-bit file ID and truncates its high 64 bits for BPF maps.

The backend implements start-time identity, executable mapping parsing,
`map_files` access, stable file IDs, PID/page entries, replacement on PID reuse,
and cleanup through notifications/sweeps. It uses a conservative observation
window rather than copying Go's processed-until mechanism. Anonymous managed
mapping monitoring remains excluded.

Procfs and executable reads enforce byte limits while reading, not just through
`stat` sizes. PID identity is read before and after mappings. The mapped device
and inode are verified for pathname fallbacks, and the file ID and ELF layout
use the same owned bytes. A successful kernel `map_files` handle is authoritative:
overlayfs can report a virtual device in `fstat` while `maps` reports its backing
device. Applying the pathname check to that handle caused 126 metadata rejections
in the first container run; the fix removed those rejections.
Metadata refreshes have a per-drain budget and bounded retries; periodic mapping
refresh uses a round-robin cursor. Each observed PID/start-time/mapping state is
admitted only after upload and a confirming procfs read. A later verification
extends its upper timestamp bound using a clock sample taken before that
observation but after raw events were copied. Unchanged-generation completion
retries preserve the admission boundary; new identities/mappings do not.

Queued records outside that observed interval are rejected and counted. Shutdown
does not perform filesystem revalidation: only already-verified intervals can be
attributed. Clock offsets are normalized to initial-namespace monotonic time,
and an isolated capture with a 1,000-second namespace shift retained deep stacks.
These rules use the observable procfs identity and mapping state. The unchanged
trace ABI contains no kernel-side generation token, so arbitrary ABA changes
between indistinguishable observations are not claimed to be detectable.

The correctness review found and fixed three independent prototype defects:
page-aligned LLD mappings could receive the wrong load bias, partially uploaded
process state could suppress future metadata retries, and the capture example
parsed CPU and duration in the wrong order. Regression coverage now includes
the real LLD offset pattern, large pages, the actual synchronizer recovering
from capacity/file failures through an in-memory map interface, and CLI parsing.

## Native unwind findings

The upstream Go implementation is a large independent subsystem:

- `.eh_frame` and `.debug_frame` parsing;
- external `.gnu_debuglink` lookup;
- Go `pclntab` parsing;
- entry-stub and signal-frame synthesis;
- unique `UnwindInfo` interning;
- merged delta encoding;
- 16 map-in-map size buckets;
- page lookup records;
- asynchronous outer-map updates;
- delayed executable eviction.

That implementation is not copied. The spike keeps ABI fidelity and supplies a
smaller frame-pointer-only plan. Lightswitch and Profile Bee confirm that
complete CFI extraction in Rust is possible, but adopting their code would
create a separate provenance, review, and maintenance project.

The measured reference subset (`nativeunwind/**/*.go`,
`processmanager/execinfomanager/*.go`, and `processmanager/ebpf/*.go`) contains
18 Go files and 5,235 lines, including tests. This excludes the ELF helper
dependencies and interpreter integration. No full CFI port was started; this
line count is a lower-bound scope measurement, not a schedule estimate.

## Event-output findings

The upstream trace ABI is little-endian and variable length:

- fixed prefix: 728 bytes;
- maximum `Trace`: 25,304 bytes;
- up to 3,072 64-bit frame words;
- leading kernel addresses counted by `num_kernel_frames`;
- user frames use a 64-bit header:
  4-bit type, 4-bit flags, 4-bit total word count, 52-bit data;
- ring output may include padding after the required words;
- `report_events` contains a four-byte event selector;
- perf buffers provide explicit lost counts;
- ring output failure is metric ID 105 in the per-CPU `metrics` map.

The Rust decoder uses byte-order functions and checked slices. It performs no
cast from arbitrary callback memory.

## Resource bounds

Default configured maxima include:

- 16 monitored CPUs and 33 perf descriptors including notification buffers and
  the lifecycle hook;
- 4,096 possible CPU slots;
- 16 KiB notification perf buffer per monitored CPU;
- 8 MiB trace ring buffer;
- 4,096 raw events per drain and 8,192 queued raw records;
- eight metadata updates per drain and an 8 MiB procfs-file read limit;
- 4,096 processes and 16,384 threads;
- 131,072 executable mappings and 4,096 executable IDs;
- 1,048,576 stack deltas and 1,048,576 unwind map entries;
- 64 MiB native unwind bytes;
- 65,536 aggregation keys and user stacks;
- 64 MiB aggregation logical bytes, including the unique-stack indexes;
- 16,384 kernel stacks;
- 512 frames per trace;
- 65,536 samples and 64 MiB logical bytes per snapshot;
- two pending snapshots;
- 64 diagnostic strings;
- one local worker and two retry attempts.

On the 16-CPU amd64 investigation host, `preflight` reports these planning
estimates:

| Class | Bytes |
| --- | ---: |
| Userspace logical storage and headroom | 4,492,026,880 |
| Kernel map estimate including transient templates | 277,544,448 |
| Ring and perf buffers including metadata pages | 9,830,400 |
| Combined estimate | 4,779,401,728 |

This is **not a proven physical-memory ceiling or expected RSS**. It includes
duplicated process/mapping state, staging reads, decoded batches, aggregation
indexes, pending/final snapshots, eight-byte array strides, all 16 outer maps,
and the largest temporary inner template. Exact allocator, BTF, verifier, and
kernel-version overhead needs measurement. The configured defaults are generous;
do not treat this estimate as a small-memory production configuration.

Snapshot truncation counts discarded samples, not just aggregate keys. A full
handoff preserves earlier generations and carries forward both rejected sample
counts and their existing kernel-loss evidence. Shutdown returns pending
generations plus the final window and releases process metadata. If the deadline
prevents establishing that kernel buffers are empty, `shutdown_incomplete` is
set: owned-queue losses are exact, but remaining kernel-record count is unknown.
The shutdown budget begins before ingress is disabled and is checked between
bounded draining, decoding, aggregation, and finalization work. Expired aggregate
samples are counted by multiplicity; undecoded records remain owned until they
are counted and released. No filesystem observation is initiated by shutdown.
Mandatory descriptor/storage release is unconditional, and its OS/allocator
latency is not a hard real-time guarantee. Capture-time filesystem operations
remain subject to normal OS I/O behavior, not an in-process forced-cancellation
promise. Interrupted reads are returned to bounded caller retry policy.

## Live Rust measurements

The workload is built with frame pointers and a 32-level non-tail-recursive call
chain. Arithmetic is kept inside the leaf, and clock checks are amortized over
1,024 iterations so the vDSO clock reader does not dominate samples.

| Run | Result |
| --- | --- |
| First one-CPU capture | 60 real events; native workload stacks up to 37 frames; exposed overlayfs metadata mismatch |
| One CPU after overlayfs fix | 59 events; stacks up to 40 frames; zero recorded losses or metadata rejections |
| Two CPUs with one pinned workload each | 117 events; both CPU attachments confirmed; both workloads had deep stacks up to 41 frames; zero recorded losses |
| Optimized 10-second capture with adaptive polling | 200 workload samples; 164 had at least 32 native frames; maximum 41; zero recorded losses; complete observed drain |
| Real startup rollback | Injected failure after map creation, native program load, tail calls, readers, lifecycle attachment, and first CPU attachment; descriptors returned to baseline each time |
| Repeated/zero-deadline shutdown | All descriptors and singleton lease released; repeated shutdown returned no new windows; incomplete-drain flag set when deadline was zero |

The optimized measurement pins the workload to CPU 0 and the Rust profiler plus
measurement helper to CPU 1. Sampling is 20 Hz on CPU 0 only. The test-only
measurement image is
`sha256:84df445b2b18099d0c84fcf85427437f1678b2e94f36ea1b5effbc19ef13ddda`,
built locally from the pinned base with Python 3.14.3, GNU time 1.9, and bpftool
7.7.0. No eBPF source/artifact or experiment binary is embedded in that image.

| Measured quantity | Result |
| --- | ---: |
| Startup | 682.7 ms |
| Shutdown | 70.5 ms |
| Steady profiler CPU, fraction of one core | 0.66% |
| Steady profiler RSS | 29,312 KiB |
| Peak child RSS | 45,808 KiB |
| Owned kernel map count, including children | 51 |
| Owned kernel map memory | 109,368,756 bytes |
| Dynamic unwind children | 4 arrays, 9,728 bytes total |
| Sampled program-memory peak | 40,960 bytes |

Map traversal starts only from the profiler child's own fdinfo IDs and follows
its map-in-map children; it does not enumerate unrelated host maps. CPU
accounting excludes helper subprocess CPU. RSS/fdinfo observations are sampled,
so they do not prove the configured worst-case bound or catch every transient
startup peak. These are short controlled measurements, not production sizing.

The earlier fixed 1 ms polling loop used about 4.58% of one core in this setup.
The pinned program uses `BPF_RB_NO_WAKEUP`, so an indefinitely blocking ring
reader would be incorrect. A configurable 10 ms timer, shortened by worst-case
ring/drain capacity and sampling rate, reduced measured steady CPU to 0.66%
without losing samples in this run.

Terminal errors are retained rather than hidden. Frame-pointer-only unwinding
typically ends with `native_pc_read` when it reaches a startup/libc frame without
a usable frame pointer or STOP rule. The earlier measurements below predate
dynamic stack-layout discovery; their rare kernel-mode register errors were
subsequently removed in the syscall-heavy recovery experiment.

### Qualification update

After adding thread-isolated kernel analysis, generation guards, and deadline
handling, a fresh ten-second capture produced 195 workload samples, 152 with at
least 32 native frames, and a maximum depth of 41. Four uncertain records were
explicitly rejected as generation mismatches; no kernel register-read error
occurred. Startup/shutdown measured 770.7/80.1 ms, steady CPU about 0.99% of one
core, steady/peak RSS 52,468/71,392 KiB, and owned map memory 109,368,756 bytes.
The additional analysis/verification changes the memory and CPU profile; the
earlier comparison rows are retained as historical measurements, not silently
relabelled as measurements of the newer qualified path.

## Go comparison

The exact pinned Go userspace was built and run separately as a reference
oracle. A disposable harness uses the public receiver factory with a direct
`reporter.Reporter` producing processed NDJSON and an in-process metrics reader.
It starts no Collector service, network exporter, or listener and never
participates in the Rust data path.

The harness was built with verified Go 1.26.0, `CGO_ENABLED=0`, a read-only local
module replacement at the pinned commit, and isolated SDK/module/build caches.
A byte search verified that its binary embeds the complete, unchanged
894,280-byte amd64 artifact. The reference checkout remained clean. Harness,
module graph, licenses, toolchain checksum, binary hashes, and raw outputs are
retained as session artifacts rather than added to the Rust repository.

Static comparison:

| Property | Upstream Go userspace | Rust spike |
| --- | --- | --- |
| Loader | cilium/ebpf | Aya |
| Artifact | Embedded by architecture | External, hash-pinned |
| Native CFI | Full `.eh_frame`, `.debug_frame`, Go | Frame-pointer commands only |
| Managed runtimes | Broad | Excluded |
| Probe chain | Rewritten and populated | Native pair prepared and initialized; perf chain exercised live |
| Output | Reporter/OTLP pipeline | Owned transport-neutral snapshot |
| Queues | Several bounded plus Go channels | Explicit bounded local queues |
| Distribution | Collector binary/images | No artifact distribution |

### Controlled comparison

Sequential runs used the same frame-pointer workload, pinned artifact revision,
20 Hz sampling on CPU 0, a profiler/measurement process on CPU 1, container PID
namespace filtering, the same measurement image, and ten seconds after readiness.
The workload SHA-256 was
`0b7174758e5af0ef5c03fbcd1f48b2886ba0207b8fd90b628fc89ea83d3474be`.
The Go build used all interpreters disabled but retained its normal native CFI
extraction. Go's trace polling is fixed at 250 ms; Rust was measured at its
normal 10 ms maximum and at a separately labelled 250 ms setting.

| Quantity | Pinned Go oracle | Rust, 10 ms | Rust, 250 ms |
| --- | ---: | ---: | ---: |
| Workload samples written | 175 | 199 | 196 |
| Samples with at least 32 native frames | 147 | 170 | 163 |
| Maximum non-error native depth | 41 | 41 | 41 |
| Startup | 841.3 ms | 643.6 ms | 589.2 ms |
| Shutdown return | 180.0 ms | 79.6 ms | 79.1 ms |
| Sampled steady CPU, fraction of one core | 1.21% | 0.65% | Below sampled tick resolution |
| Steady RSS | 41,432 KiB | 29,284 KiB | 29,308 KiB |
| Peak child RSS | 47,364 KiB | 45,684 KiB | 45,804 KiB |
| Owned maps, including inner maps | 41 | 51 | 51 |
| Owned kernel map memory | 35,094,740 bytes | 109,368,688 bytes | 109,368,688 bytes |
| Trace ring capacity | 524,288 bytes | 8,388,608 bytes | 8,388,608 bytes |

The 250 ms Rust run still consumed CPU: total measured user/system time was
0.0276/0.5637 seconds, including startup and shutdown. A zero delta in sampled
clock ticks during its steady interval is not a claim of zero CPU cost.

### Stack and sample-count fidelity

Go and Rust agree on the dominant normalized location:
file ID high half `eefd395e5533bd59`, ELF address `92133`. The two Rust runs
shared 16 and 17 exact normalized locations with Go, respectively. Rust return
addresses were decremented by one when its return-address flag was present,
matching Go's conversion. No ASLR addresses were compared directly.

The sample-count difference is substantially explained by observation windows,
not hidden as a claimed Rust sampling improvement. A follow-up Go timing run
recorded its first sample 1,040.7 ms after readiness, covered an 8.70-second sample
span, and recorded its last sample 467.2 ms before shutdown returned. An interior
five-second window contained 99 samples against 100 nominally expected at 20 Hz.
Go's one-second PID monitoring, 250 ms trace polling, and lack of a final
ring-drain/join barrier differ from Rust's startup synchronization and final drain.

Go's comparison run contained one `native_no_pid_page_mapping` error and reported
related semantic-error metrics; no ring/perf buffer-loss metric was reported.
Its metric flush covers previously polled userspace values, not a final BPF-map
read, so absent loss metrics do not prove exact zero loss at the shutdown tail.
Rust recorded no buffer/capacity/metadata losses but retained 198 and 195
`native_pc_read` terminal errors in the two runs, plus one missing-mapping error
each. Its FP-only plan does not provide Go's native CFI/STOP rules. Go's first
reference run terminated all workload stacks without an error frame.

The Go reporter exposes PID/TID/path but not the Rust snapshot's PID start-time
identity. Only workload-attributed records from each isolated namespace were
compared; equal numeric PIDs across different containers were not assumed.

### Why these are not language-performance claims

The configurations are intentionally reported, not called equivalent:

- Go performs full ELF CFI extraction; Rust installs FP commands only.
- The direct Go oracle writes every processed sample as NDJSON; Rust aggregates
  and hands off owned snapshots, so output work differs.
- Go uses 65,536 stack-page entries; Rust currently uses 1,048,576.
- Go outer stack-delta maps have 65,536 slots with `BPF_F_NO_PREALLOC`; Rust uses
  4,096 preallocated slots. PID/page capacities are equal at 1,048,576.
- Go's ring uses its `runtime.NumCPU()` value, which was one under profiler
  affinity, rather than the selected sampling-CPU count. Rust uses an explicit
  8 MiB limit.
- Matching the 250 ms setting does not make timer/ticker or shutdown scheduling
  identical. CPU figures cover userspace, not eBPF CPU time or workload slowdown.

The evidence demonstrates interoperable native addresses and a working Rust
control plane, not that removing Go is universally faster or smaller.

The checked-in workload can be built with:

```bash
RUSTFLAGS="-C force-frame-pointers=yes" \
  cargo build \
  -p otel-arrow-dfe-upstream-ebpf-profiler-backend \
  --example workload \
  --release
```

Run the resulting executable under each profiler separately. It uses a fixed
32-level non-inlined call chain and deterministic arithmetic. Post-recursion
work prevents the optimizer from turning it into a tail-call loop.

## Local microbenchmarks

Measured on 2026-09-05, release profile, two build jobs, 100 ms warmup,
500 ms measurement, and 20 Criterion samples. These short runs characterize
synthetic Rust operations only; they do not compare Go or measure profiler CPU,
RSS, or live loss rates. Intervals below are the reported timing intervals.

| Operation | Input | Time |
| --- | --- | --- |
| Repeated-stack aggregation | 1,024 samples, one frame each | 139.40--149.11 us |
| Executable mapping lookup | Last of 64 load segments, corrected page handling | 91.331--92.780 ns |
| Unwind metadata encoding | 64 executable segments | 1.2846--1.3840 us |
| Trace/frame decoding | 1 native frame | 48.354--49.557 ns |
| Trace/frame decoding | 32 native frames | 0.98662--1.0340 us |
| Trace/frame decoding | 128 native frames | 4.8613--4.9345 us |
| Trace/frame decoding | 512 native frames | 18.726--19.788 us |
| Window finalization | 256 distinct 32-frame stacks | 157.15--164.07 us |
| Owned snapshot handoff | 256 distinct 32-frame stacks | 71.631--251.02 ns |

The handoff measurement is noisy. Repeat longer measurements on an otherwise
idle host before making performance decisions.

The mapping-lookup row was remeasured on 2026-09-06 after correcting executable
page selection and checking ambiguous aliases. The previous implementation took
46.927--49.874 ns on this synthetic case but could compute an incorrect load
bias for real LLD executables. The additional correctness checks increased this
microbenchmark's cost; no end-to-end overhead conclusion follows from it.

## Remaining acceptance work

Map creation, verifier load, attachments, real events, deep native stacks,
multi-CPU operation, kernel-mode recovery, conservative generation attribution,
tested rollback/deadline handling, measured map/RSS evidence, and the Go comparison
now exist. Remaining qualifications are explicit: full CFI and managed runtimes,
live arm64 and broader kernel coverage, deployment/legal approval, and hard
real-time or physical allocator guarantees are not supplied by this experiment.

The native probe binding gate has both non-privileged equivalence and live
initialization evidence, without an upstream source patch or scope waiver. Perf
tail-call execution is demonstrated; custom-probe execution remains excluded.
Nothing in this worktree is a DFE receiver, and the independent-profiler
worktree remains untouched. Publication is authorized only on the separate
experimental branch in the `lalitb/otel-arrow` fork. No PR, upstream repository
update, or change to main or the independent-profiler branch is part of this work.

## Experiment handoff

This is a milestone report, not a declaration that the full definition of done
has been met. It covers all requested reporting topics without substituting
synthetic results for live evidence.

| # | Topic | Outcome |
| --- | --- | --- |
| 1 | Starting commit | Existing `origin/main` at `fa32548c6f0b5ffca8c414bd2c820f57d6102990`; no fetch or rebase |
| 2 | Branch and worktree | `ebpf-profiler/02-upstream-kernel-rust-userspace`, `/home/labhas/personal/profiles-df-engine/otel-arrow-upstream-ebpf-spike` |
| 3 | Upstream revision and hashes | `06ea040c39d3d17bc1534a5dcc044368caf48782`; original and prepared amd64/arm64 SHA-256 values are in [COMPATIBILITY.md](COMPATIBILITY.md) |
| 4 | Inventory | 38 programs, 47 maps, 23 globals, and 13 slots in each of two program arrays; both architecture manifests included |
| 5 | Native dependency graph | Documented above: four-program perf minimum, plus two native probe targets using separate scratch and tail-call maps |
| 6 | Loader choice | Aya 0.14; installed libbpf rejects static entry symbols in the unmodified object; no second production loader retained |
| 7 | Feasibility | Tested Linux amd64 native FP path works; broader qualification remains incomplete |
| 8 | Rust architecture | Artifact validation, checked preparation, typed Aya maps/programs, bounded reads/drains/metadata/aggregation, and owned snapshots |
| 9 | Upstream reuse | External artifacts/headers; six sampler REL bindings plus one separately pinned analysis instruction-byte patch; no upstream file edits |
| 10 | Licensing | Apache-2.0 userspace/GPL-2.0 eBPF boundary documented; no artifact distribution; human review still required before packaging |
| 11 | Supported evidence | Artifact/ABI/relocation equivalence, real one-/two-CPU capture, deep native stacks, metadata recovery, bounded aggregation, and tested rollback/shutdown |
| 12 | Exclusions | DFE/OTAP/Arrow/OTLP integration, full CFI and managed runtimes, symbolization, custom probe attachment, off-CPU/memory profiling, and correlation |
| 13 | Resource limits | Explicit limits; approximately 109.37 MB in 51 live owned maps, 29 MiB steady RSS, 45 MiB peak RSS in the measured setup; worst-case planning estimate is not a physical guarantee |
| 14 | Files changed | New `crates/upstream-ebpf-profiler-backend/` crate, examples, tests, benchmarks, manifests and five documents; workspace Cargo.toml/Cargo.lock only outside that directory |
| 15 | Validation | 96 non-privileged acceptance cases, two live concurrency/rollback/deadline cases, native Clippy, benchmark checks, and the complete serialized workspace check pass |
| 16 | Privileged results | Namespace-filtered captures, shifted-clock capture, concurrent analysis, eight startup fault points, and repeated/zero-deadline shutdown are exercised in auto-cleaned containers |
| 17 | Go comparison | Exact-source offline oracle run separately; normalized addresses agree, with explicit differences in CFI, polling, output, sample windows, and map budgets |
| 18 | Maintenance risks | Two pinned preparation policies, kernel coverage, observation-based identity limits, physical allocator/scheduler limits, and distribution policy |
| 19 | Recommendation | Continue only as the isolated experiment until live gates pass; do not replace or redirect the independent profiler |

### Validation notes

The earlier prototype passed `cargo xtask quick-check`, `cargo xtask
check-benches`, and the full `cargo xtask check`. After the review fixes and
probe-preparation integration, focused tests and native Clippy pass, and
`check-benches` passes again. The first full rerun encountered the unchanged
core-node test
`exporters::otap_exporter::tests::test_shutdown_nacks_correlated_pdata`
with `pipeline result channel closed`, and a second full run reproduced it.
The test uses a 10 ms shutdown deadline. It passed both an isolated Cargo run
and direct invocation from the same full-workspace test binary.

No exporter code or existing dependency version was modified. The default
parallel full check is recorded as failing, not silently waived. A separate
workspace run passed with only this named test skipped, and the same
full-workspace binary passed that test when invoked alone. This is not described
as a passing canonical full check.

After the live-path changes, the complete suite passed with
`RUST_TEST_THREADS=1 cargo xtask check`, with no additional skipped tests.
`cargo xtask check-benches` also passed. Serialized test execution avoids the
observed cross-test timing interference; it does not fix the unrelated 10 ms
deadline assumption.

The final focused run passed 96 non-privileged cases, including the independent
C ABI fixture, both-architecture preparation policies, metadata/generation
recovery, clock offsets, deadlines, and capture arguments. Two explicitly opted-in
live cases passed separately: concurrent-thread analysis and eight-stage
rollback/repeated/expired-backlog shutdown. Native-feature Clippy, benchmark
checks, and the complete serialized `cargo xtask check` passed again.

The final focused read-only review of the request-filter, generation-retry, and
deadline wiring changes found no significant issues. Markdown and sanity checks
also passed, including new files through a temporary index without staging the
real worktree. These results establish the stated bounded-work native-FP
contract; they do not add an all-kernel, hard real-time, or unobservable
kernel-generation-token guarantee.
