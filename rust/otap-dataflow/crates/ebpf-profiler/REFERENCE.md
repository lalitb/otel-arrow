# Profiler reference analysis

This document records behavioral evidence used to design the independent Rust
profiler. The references are not implementation foundations, runtime
dependencies, or sources of copied code, constants, fixtures, generated
objects, or coredumps.

## Revisions and licenses

| Reference | Revision studied | Relevant license boundary |
| --- | --- | --- |
| OpenTelemetry eBPF profiler HEAD | `06ea040c39d3d17bc1534a5dcc044368caf48782`, `v0.0.202633-63-g06ea040` | Apache-2.0 repository; GPL-2.0 kernel source in `support/ebpf/LICENSE` |
| Collector-pinned Go profiler | `v0.0.202633`, commit `0d228017e8f64d68e2fe68b78ac6db0297f04b22` | Same Apache/GPL source boundary |
| Collector releases | `1ce63a79ef35c4f858bfbdb3ae0025f071f3bdf4`; manifest pins `go.opentelemetry.io/ebpf-profiler v0.0.202633` | Apache-2.0 |
| Lightswitch | `a9c1d0cc8c8c8b0eebdfa51e5b9b0ca43e358766`, `v0.5.0-44-ga9c1d0c` | MIT Rust crates; kernel source metadata needs human review because SPDX and runtime license strings differ |
| Profile Bee | `b94ee599f0e03ee1e3273930a1bee9d2370da8bc`, `v0.3.24` | MIT repository; release-license packaging should be reviewed |
| Target base | `fa32548c6f0b5ffca8c414bd2c820f57d6102990` | Apache-2.0 user space; original GPL-2.0-only kernel source |

The Go pinned tag is 63 commits behind the studied local HEAD. Its core
sampling, transport, map architecture, native unwind model, licensing, and
incomplete final-flush behavior are materially unchanged.

## Classification vocabulary

- **Adopt conceptually**: retain the problem-solving idea, independently
  implemented.
- **Adapt to bounded resources**: retain the mechanism only with explicit
  capacities, loss policy, and telemetry.
- **Improve**: use the reference as evidence while correcting lifecycle,
  correctness, or operability issues.
- **Simplify**: implement a smaller mechanism sufficient for this milestone.
- **Defer**: preserve an extension seam but do not implement it now.
- **Reject**: do not carry the behavior into this library.

## Lifecycle

### Go profiler

The Go controller starts reporter connectivity, kernel state, PID handling,
perf events, probes, and event monitors in a fixed order
(`internal/controller/controller.go:59-157`). Perf-event attachment closes
previous CPU FDs after a later failure (`tracer/tracer.go:1180-1222`), but the
complete tracer startup is not transactional and the standalone executable
does not run shutdown after `Start` fails (`main.go:121-132`).

Shutdown cancels processing, stops the reporter, and closes the tracer
(`internal/controller/controller.go:160-175`). Reporter stop closes a channel
without a synchronous final aggregate export (`reporter/base_reporter.go:44-46`;
`reporter/runloop.go:43-45`). Readers and workers are not generally joined.

**Classification: Improve.** Preserve ordered startup and per-CPU rollback, but
use explicit RAII ownership, worker handshakes, producer detachment before
drain, a bounded final window, joined workers, and repeatable deadline-aware
shutdown.

### Lightswitch

Lightswitch opens and loads skeletons, attaches perf events and tracers, and
starts detached pollers and a collector (`src/profiler.rs:246-338,422-496`).
`expect`, a process-exiting panic hook, discarded join handles, and manual
skeleton lifetime manipulation make this unsuitable for an embedded library
(`src/cli/main.rs:60-75`; `src/bpf_objects.rs:34-48,52-130`;
`src/bpf_poller.rs:24-72`). Its final profile is asynchronously queued without
acknowledgement before collector finish (`src/profiler.rs:504-515,567`).

**Classification: Reject** detached and panic-driven lifecycle behavior.
**Adopt conceptually** the explicit open/load/attach phases.

### Profile Bee

Profile Bee loads and attaches before spawning a target and can fall back from
DWARF to frame pointers (`profile-bee/bin/profile-bee.rs:889-929`;
`profile-bee/src/ebpf.rs:496-521`). Later setup failure can strand the spawned
target because its asynchronous kill future is not awaited
(`profile-bee/src/spawn.rs:112-121,168-171`). Event draining stops while
programs remain attached during later processing
(`profile-bee/src/event_loop.rs:825-841`).

**Classification: Improve.** Keep graceful mode fallback, but require
transactional target/resource ownership and a detach-drain-finalize barrier.

## Perf events, CPUs, and topology

All three references use software CPU-clock frequency sampling. Go opens one
system-wide event for each online CPU and supports probabilistic enable periods
(`tracer/tracer.go:1180-1299`). Lightswitch also uses one disabled event per
online CPU (`src/perf_events.rs:8-23`). Profile Bee supports host-wide, PID, and
selected-CPU scopes (`profile-bee/src/ebpf.rs:489-576`).

The independent implementation adopts one host-wide perf event per selected
CPU. It validates the selected count, the fixed event-map CPU-ID range, and
uses strict transactional or best-effort attachment. Topology is currently a
startup snapshot; CPU hotplug reconciliation is deferred.

Cost scales with selected CPUs, frequency, and stack depth. Future
configuration should add an aggregate host sampling budget rather than
validating only per-CPU frequency.

**Classification: Adopt conceptually**, with bounded selection, sparse-ID
validation, rollback, and future hotplug reconciliation.

## Kernel objects and loader boundaries

Go builds and commits architecture-specific ELF objects, embeds them, rewrites
constants and map references, and verifies deterministic rebuild hashes in CI
(`support/support_amd64.go:1-13`; `support/support_arm64.go:1-13`;
`.github/workflows/unit-test-ebpf.yml:18-43`).

Lightswitch uses `libbpf-cargo` skeleton generation and demonstrates open/load
separation, `.rodata`, custom BTF, map-FD reuse, and map-of-maps templates
(`build.rs:38-79`; `src/bpf_objects.rs:52-163`).

Profile Bee separately builds Rust eBPF and embeds fallbacks
(`xtask/src/build_ebpf.rs:43-63`; `profile-bee/build.rs:21-56`). Its release
workflow provides evidence that architecture selection must be explicit rather
than inferred from the newest object.

The independent build emits a separate object and manifest containing:

- manifest format and ABI version;
- target architecture;
- source and object SHA-256 digests;
- compiler identification;
- expected program and map names;
- kernel-source license.

The loader bounds object and manifest sizes and verifies architecture, ABI,
names, and digest before Aya parses the ELF. Objects remain generated rather
than checked in during incubation. A production release should reproducibly
embed per-architecture objects while retaining an explicit diagnostic override.

**Classification: Improve.**

## Aya versus libbpf-rs

| Dimension | Aya evidence | libbpf-rs evidence |
| --- | --- | --- |
| Deployed dependencies | Pure-Rust user-space loader without deployed libbpf/libelf | Adds libbpf, libelf, zlib, and native integration |
| Simple perf sampling | Aya supports perf-event programs and per-CPU perf buffers directly | Fully supported but heavier than needed |
| Typed object contract | Program and map names are runtime strings | Generated skeletons provide typed maps, programs, globals, and phases |
| CO-RE | Aya can consume Clang relocation records; Profile Bee manually resolves offsets for its Rust program | Lightswitch directly demonstrates Clang CO-RE, custom BTF, and feature probes |
| Map-of-maps and batch APIs | Profile Bee uses an Aya fork and direct syscalls for some required behavior | Mature libbpf APIs and templates |
| Async integration | Aya FDs work directly with async polling | Usually needs a polling thread or adapter |
| Ownership complexity | Straightforward for this single simple object | Lightswitch demonstrates wrapper complexity around generated skeleton lifetimes |
| Build model | No native runtime dependency; Rust eBPF needs extra nightly/bpf-linker tooling | Heavier toolchain, but explicit object architecture and CO-RE generation |

**Decision: retain Aya 0.14 for the helper-only, fixed-ABI milestone.** It
avoids unjustified native runtime dependencies and supports the required perf
programs, maps, buffers, x86_64, and aarch64. The kernel source is C because the
main workspace uses stable Rust and ordinary checks must not require a separate
nightly BPF toolchain.

Advanced `.eh_frame` work must prototype right-sized map-of-maps, batch updates,
map reuse, custom BTF, verifier diagnostics, and reproducible embedding. If Aya
requires a private fork or direct `SYS_bpf` implementation for production
primitives, prefer libbpf-rs rather than owning another runtime layer.

**Classification: Aya now - Adopt conceptually. Advanced Aya backend - Defer.
libbpf-rs fallback - Adapt if CO-RE and map-of-maps dominate.**

## Event transport and loss

Go splits high-volume trace data onto a ring buffer and process notifications
onto a perf array. PID identity also lives in a coalescing kernel map so a lost
notification can be recovered (`support/ebpf/tracemgmt.h:170-212,602-626`).
A nil channel item serves as a timestamp barrier (`tracer/events.go:252-284`),
but the unbuffered trace channel couples kernel draining to processing.

Lightswitch selects ring buffers when supported and perf buffers otherwise
(`src/cli/main.rs:287-309`). Some output failures are counted, but poller death
and several control-event losses are not propagated.

Profile Bee performs much aggregation in a kernel `COUNTS` map and uses a small
ring largely for notification (`profile-bee-ebpf/src/lib.rs:340-435,529-551`).
Map, stack, and ring failures are often ignored.

The independent implementation uses fixed 1,072-byte events in per-CPU perf
buffers. This preserves CPU-to-shard locality and exposes kernel loss at the
cost of fixed bandwidth for shallow stacks. A per-CPU map supplies scratch
storage because the event exceeds the BPF stack limit. Wrapped perf records are
size-checked before allocation.

Workers aggregate locally and use bounded nonblocking channels. Kernel loss,
malformed records, raw capacity, shard-window capacity, completed-snapshot
capacity, disconnected consumers, and shutdown deadlines have fixed counters.

**Classification: Adapt to bounded resources.** A future advanced unwinder can
evaluate split control/data transport or ring buffers without weakening loss
accounting.

## Native stacks and advanced unwinding

Go converts Go tables, `.eh_frame`, `.debug_frame`, and debug links into compact
interval deltas, deduplicates rules, and uses bounded kernel binary searches
and tail calls (`nativeunwind/elfunwindinfo/stackdeltaextraction.go:112-195`;
`processmanager/execinfomanager/manager.go:409-545`;
`support/ebpf/native_stack_trace.h:27-153`).

Lightswitch emits compact rows partitioned by code pages and bounds binary
search (`lightswitch-unwind-info/src/convert.rs:173-218,290-383`;
`lightswitch-unwind-info/src/pages.rs:21-87`). Its current initial-register
failure handling can continue with stale per-CPU state
(`src/bpf/profiler.bpf.c:629-654,677-684`).

Profile Bee supports frame-pointer and DWARF modes but allocates a fixed large
inner map per loaded binary and can omit unsupported CFI regions without an
invalid-gap marker (`profile-bee/src/ebpf.rs:1005-1039`;
`profile-bee/src/dwarf_unwind.rs:446-483`).

The first milestone guarantees frame-pointer-enabled native user stacks through
the kernel stack helper. Advanced native unwinding is deferred and must add:

- global and per-process byte and row budgets;
- right-sized or shared maps with deterministic eviction;
- invalid entries for unsupported rule gaps;
- bounded parser input, operations, and wall time;
- x86_64 and aarch64 fixtures including PAC, signals, leaf LR, PLT, PIE,
  stripped and deleted objects;
- observable extraction, fallback, lookup, tail-call, and eviction failures.

**Classification: Adapt to bounded resources.**

## Processes, mappings, and symbols

Go tracks process and executable lifecycle from kernel notifications, shares
unwind data by file identity, reference-counts it, and delays metadata deletion
behind timestamp barriers (`process/process.go:295-444`;
`processmanager/processinfo.go:251-370,480-530`;
`processmanager/execinfomanager/manager.go:239-292`). It still documents PID
reuse races.

Lightswitch uses `map_files`, namespace roots, build IDs, and text hashing
(`src/profiler.rs:1341-1479`; `src/process.rs:165-173`;
`lightswitch-object/src/object.rs:127-147`). Its mapping lifecycle is incomplete
for partial unmaps and PID reuse.

Profile Bee has a bounded metadata cache keyed by PID and procfs start time, but
other caches remain PID-only and lifecycle rings can lose ordering
(`profile-bee/src/process_metadata.rs:48-51,130-197`;
`profile-bee/src/event_loop.rs:764-814`).

The independent profiler keys process metadata by PID and procfs start ticks,
refreshes cached metadata at a bounded one-second cadence, caps procfs bytes and
strings, parses executable mappings, normalizes addresses, and keeps bounded
per-file symbol indexes. Each reporting window owns its metadata graph.

Event-driven exec/exit/mmap/munmap/mprotect tracking, mapping generations,
build IDs, durable object FDs, and timestamp-barrier retirement are future work.

**Classification: Improve.**

## Aggregation identity

Go aggregation includes resource, profile type, trace, thread, CPU, trace/span
IDs, and metadata, but retains unbounded timestamp/value vectors
(`reporter/base_reporter.go:49-108`; `reporter/samples/samples.go:33-99`).

Lightswitch aggregates by a 64-bit hash without full equality verification,
allowing collisions to merge unrelated samples (`src/profile/sample.rs:147-169`;
`src/aggregator.rs:12-35`).

Profile Bee includes transient register and command fragments in its kernel key,
which fragments logically identical stacks (`profile-bee-common/src/lib.rs:9-39`).

The independent profiler uses full equality over process generation, thread,
interned user stack, optional kernel stack, and CPU. Shard finalization
deduplicates process, thread, mapping, function, location, and stack rows, then
reaggregates equal samples. CPU is an explicit dimension rather than a
last-observed annotation.

**Classification: Improve.**

## Resource bounds and backpressure

The Go reporter tree has no firm bound on resources, stacks, labels, or
aggregation memory. Lightswitch uses unbounded sample, event, and profile
channels and an effectively unlimited default unwind budget. Profile Bee uses
several unbounded standard channels and long-lived collections.

The independent implementation retains the reference-independent bounded
model:

- exact global capacities are partitioned across shards without ceiling
  duplication;
- pending shard capacity is enforced and estimated as individual snapshots,
  not generations;
- completed snapshots use a bounded nonblocking channel;
- file reads, strings, tables, errors, and object files are bounded;
- total memory uses checked arithmetic and a conservative overhead margin.

**Classification: Reject** unbounded reference behavior. **Adapt to bounded
resources** where their mechanisms are otherwise useful.

## Test and benchmark lessons

Go has the strongest compatibility matrix: amd64 and arm64 tests, deterministic
object hash checks, and QEMU kernels across multiple versions
(`.github/workflows/unit-test-on-pull-request.yml:91-115,178-246`;
`.github/workflows/unit-test-ebpf.yml:18-43`).

Lightswitch has useful no-frame-pointer C++, Go, OCaml, custom-BTF, and helper
fallback fixtures (`tests/integration_test.rs:147-391`). Profile Bee has useful
frame-pointer, DWARF, PIE, shared-library, Rust, V8, Bun, and off-CPU e2e
workloads (`tests/run_e2e.sh:650-692`).

The independent crate uses non-root tests for configuration arithmetic, ABI
validation, topology, sharding, procfs races, mappings, symbol bounds and
caching, aggregation, PID reuse, snapshot ownership, singleton lifecycle,
kernel loss, slow/disconnected consumers, and shutdown. An opt-in real-kernel
test and Criterion benchmarks cover the first milestone.

Future CI should add deterministic object rebuild comparison and a QEMU matrix
from the declared minimum kernel through current x86_64 and aarch64 kernels.

**Classification: Adopt conceptually**, extended with bounded overload and
lifecycle fault injection.

## Meaningful Go HEAD changes after `v0.0.202633`

- Go 1.25 to 1.26 and dependency updates.
- Explicit adversarial-input bounds for HotSpot and .NET parsers
  (`interpreter/hotspot/data.go:28-38`;
  `interpreter/dotnet/instance.go:145-176`). **Adopt conceptually.**
- Extensible process metadata enrichers invoked outside the process-manager
  lock (`processmanager/processinfo.go:120-160`). **Adapt** with deadlines and
  capacity limits.
- Standard Collector probe extensions replacing older controller ownership.
  **Adopt conceptually** outside profiler core.
- Generic uprobes and modular off-CPU extensions. **Defer.**
- ARM64 Go `runtime.asmcgocall` recovery and corrected independent x86
  register-held return-address/frame-pointer recovery. **Adopt** in future
  advanced unwinding.
- Correct Linux device encoding and deleted-executable normalization
  (`process/process.go:108-117,374-388`). **Adopt.**
- Race-free process-manager size telemetry. **Adopt.**
- Process Context attributes propagated to output. **Adapt** as bounded,
  optional snapshot resource identity.
- Positive interval validation and suppression of polling with no observer.
  **Adopt.**
- Additional LuaJIT, Ruby, .NET, and Python behavior. **Defer.**

## Complete Go configuration inventory

The receiver is named `profiling` and supports Linux amd64/arm64. The following
inventory describes the studied Go HEAD, not configuration accepted by this
Rust crate. The complete declarations/defaults are in
`collector/config/config_linux.go:51-81` and
`collector/factory_linux.go:39-54`; pinned declarations are at
`v0.0.202633:collector/config/config_linux.go:60-94`.

| Receiver field | Default | Validation and behavior |
| --- | --- | --- |
| `reporter_interval` | `5s` | HEAD requires positive duration; pinned did not. Aggregation/export cadence. |
| `reporter_jitter` | `0.2` | Range `[0,1]`; zero disables report jitter. |
| `monitor_interval` | `5s` | HEAD requires positive duration; pinned did not. PID/metrics polling. |
| `samples_per_second` | `20` | At least one; no upper bound. Applied per selected CPU. |
| `frame_cache_size` | `16384` | LRU range 1,024 through 1,048,576 entries. |
| `probabilistic_interval` | `1m` | Range one through five minutes. |
| `probabilistic_threshold` | `100` | Range 1 through 100; percentage of sampling intervals enabled. |
| `clock_sync_interval` | `3m` | No validation; nonpositive values leave startup synchronization only. |
| `send_error_frames` | `false` | Default filters error frames and error-only traces. |
| `send_idle_frames` | `false` | Default skips PID 0 idle samples. |
| `filter_min_process_age` | `0` | Nonnegative; zero disables age filtering. |
| `verbose_mode` | `false` | BPF debug output; Collector log level is configured separately. |
| `include_env_vars` | empty string | Comma-separated names; no explicit count/value-byte cap. |
| `map_scale_factor` | `0` | Range 0 through 8; exponentially scales selected map capacities. |
| `bpf_verifier_log_level` | `0` | Range 0 through 2. |
| `no_kernel_version_check` | `false` | Default checks Linux 5.10+; bypass does not supply missing helpers. |
| `max_grpc_retries` | `5` | No validation; standalone/custom OTLP reporter, not the default Collector consumer. |
| `max_rpc_msg_size` | `32 MiB` | No validation; does not split default Collector output. |
| `bpf_fs_root` | `/sys/fs/bpf/` | OBI context map pin/load root; CLI spelling is `-bpffs-root`. |
| `error_mode` | `propagate` | Case-insensitive `ignore` or `propagate`; ignore cleans up a failed profiler and permits Collector startup. |
| `obi_process_ctx` | `false` | Shared OBI trace/span map; disabled map shrinks to one entry. |
| `pid_namespace_translation` | `false` | Restricts/translates to profiler PID namespace; requires corresponding kernel metadata/helper support. |
| `pin_cpu_ids` | empty string | CPU-list syntax; empty means online CPUs, otherwise intersects with online CPUs. |
| `interpreters` | all enabled | Nested runtime flags described below. |
| `probes` | empty | HEAD uses IDs of started probe extensions. Pinned used inline mappings. |
| `off_cpu_threshold` | pinned only, `0` | Pinned range `[0,1]`; removed at HEAD in favor of `offcpu` extension threshold. |
| `load_probe` | pinned only, `false` | Pinned generic probe unwinder loading; removed at HEAD. |

Validation is in `collector/config/config_linux.go:86-169`. Relevant runtime
effects are in `tracer/tracer.go:666-749,1127-1298`,
`tracer/systemconfig.go:554-617`, and `times/times.go:103-113`.
HEAD additionally collects `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES`
as Process Context fallback attributes independently of `include_env_vars`
(`process/processcontext/processcontext.go:157-203,320-347`).

All interpreter `disabled` fields default to false: `python`, `perl`, `php`,
`hotspot`, `ruby`, `v8`, `dotnet`, `go`, `beam`, and `luajit`. Go also has
`labels.disabled` and `symbolization.disabled`; parent `go.disabled` overrides
both. Ruby has `skip_native_resume=false`; setting it reduces tail calls but
loses native frames inside Ruby C functions. Native ELF unwinding has no
disable field. LuaJIT's presence in configuration does not imply operational
support. See `interpreter/config.go:6-17`,
`interpreter/interpreterconfig/config.go:22-54`,
`interpreter/go/config.go:10-34`, and `interpreter/ruby/config.go:10-17`.

HEAD `kprobe` extensions accept `mode` (default `kprobe`, also `kretprobe`,
`uprobe`, `uretprobe`), required `symbol`, and `target` for user probes.
The dedicated `uprobe` extension requires `target` and `symbol`.
The `offcpu` extension requires `threshold` in `(0,1]`. Extensions must be
started under `service.extensions`. Pinned inline `probes` instead used
`{type, mode, symbol, target}`, with only `type: kprobe` dispatched.
Sources: `probes/kprobe/kprobe.go:21-69`, `probes/uprobe/uprobe.go:4-81`,
`probes/offcpu/offcpu.go:22-37`, and
`v0.0.202633:internal/controller/controller.go:176-203`.

### Standalone flags and a 19 Hz baseline

The standalone executable is development/testing tooling rather than the
supported deployment path (`README.md:45-76,90-101`; `main.go:58-67`).
Both studied revisions expose these flags:

| Flag | Default |
| --- | --- |
| `-bpf-log-level` | `0` |
| `-collection-agent` | empty |
| `-copyright` | `false` |
| `-disable-tls` | `false` |
| `-filter-min-process-age` | `0` |
| `-frame-cache-size` | `16384` |
| `-map-scale-factor` | `0` |
| `-monitor-interval` | `5s` |
| `-clock-sync-interval` | `3m` |
| `-no-kernel-version-check` | `false` |
| `-pin-cpu-ids` | empty/parser-backed CPU list |
| `-pprof` | empty listening address |
| `-probabilistic-interval` | `1m` |
| `-probabilistic-threshold` | `100` |
| `-reporter-interval` | `5s` |
| `-reporter-jitter` | `0.2` |
| `-samples-per-second` | `20` |
| `-send-error-frames` | `false` |
| `-send-idle-frames` | `false` |
| `-t`, `-tracers` | `all` |
| `-v`, `-verbose` | `false` |
| `-version` | `false` |
| `-env-vars` | empty |
| `-bpffs-root` | `/sys/fs/bpf/` |
| `-obi-process-ctx` | `false` |

The parser also accepts `-config` for its plain-text configuration format and
uses the `OTEL_PROFILING_AGENT` environment prefix. Pinned-only flags removed at
HEAD are `-off-cpu-threshold=0`, repeatable `-probe-link`, and
`-load-probe=false`. Sources: `cli_flags.go:23-185` and
`v0.0.202633:cli_flags.go:23-215`.

`-tracers` accepts `all`, `python`, `perl`, `php`, `hotspot`, `ruby`, `v8`,
`dotnet`, `go`, `labels`, `beam`, `luajit`, and deprecated no-op `native`.
Explicit `go` enables symbolization but not labels; `labels` enables labels but
not symbolization. Explicit `luajit` warns about incomplete support
(`cli_flags.go:198-249`).

With a separately configured local OTLP consumer and suitable privileges, the
actual Go standalone flags for continuous 19 Hz sampling are:

```sh
./ebpf-profiler \
  -samples-per-second=19 \
  -probabilistic-threshold=100 \
  -collection-agent=127.0.0.1:11000 \
  -disable-tls
```

Keep CPU scope, report interval, jitter, and workload identical when comparing.
The Rust comparison harness accepts an explicit argv array; it does not start
this baseline automatically or supply its OTLP endpoint.

The Collector equivalent sets `receivers.profiling.samples_per_second: 19` and
`probabilistic_threshold: 100` in YAML. Its executable uses generic
`--feature-gates=+service.profilesSupport --config <file>` rather than a
profiler-specific sampling CLI flag.

The standalone Go executable supplies a no-op meter. At HEAD, BPF/process
metrics are therefore not polled or exported; pinned still polled/reset them
without exporting. Use an appropriately instrumented Collector baseline when
loss/occupancy metrics are required. Do not treat absent metrics as zero
(`metrics/metrics.go:21-71`; `tracer/tracer.go:1148-1163`; `main.go:113-118`).

## Go runtime support matrix

These are reference capabilities, not features of the new Rust crate.
Active unwinders target Linux x86_64/aarch64, with the limitations below.

| Runtime | Mechanism and supported range | Important limitations |
| --- | --- | --- |
| C/C++/Rust native | ELF `.eh_frame`, `.debug_frame`, debug links, compact kernel stack deltas | Needs supported CFI or frame-pointer rules; unsupported expressions become invalid deltas. |
| Go | pclntab/buildinfo; stripped, static, PIE, CGo; plugins defer to main executable | Error text says 1.13-1.27, but code explicitly rejects 1.28+; unknown version fallback is 1.16. ARM64 rules differ across Go 1.20/1.21. |
| JVM/HotSpot | VMStruct/VMType/JVMCI, interpreted/JIT nmethod, live lines/inlining; JDK 7+ | Required introspection exports; AOT outside CodeCache unsupported. |
| CPython | 3.6-3.14; BPF Python frames plus live code/line metadata | CPython, not generic PyPy; cold interpreter-range recovery is x86-only. |
| Node/V8 | V8 8.1+, Node/Nsolid/libnode, JIT/bytecode and inline metadata | Needs V8 metadata symbols; no WebAssembly/async callers or universal builtin line data. |
| .NET/CoreCLR | CoreCLR 6-10, JIT, MethodDef/IL offsets, ReadyToRun, x64/arm64 PE | No NativeAOT or Edit-and-Continue; limited inlining/lines; OSR can duplicate frames. |
| Ruby | 2.5 through below 4.1, architecture/version offsets and VM stacks | Stripped 3.3+ can fail; HEAD YJIT frames are coarse and cannot resume native unwinding. |
| PHP | 7.3-8.5 CLI/CGI/FPM/libphp; PHP 8 OPCache Hybrid JIT | Required runtime symbols; other JIT modes or missing synthetic return address yield incomplete stacks. |
| Perl | 5.28-5.42, threaded/non-threaded context stacks | No `Perl_runops_debug`; current COP file/line, not definition line. |
| Erlang/BEAM | OTP 27/28 JIT module/function/arity and lines | Requires static symbol `r`; stripped BEAM can fail; anonymous executable mappings are assumed BEAM JIT. |
| LuaJIT | Non-operational scaffolding at both revisions | Loader returns no instance; BPF unwinder is an unreachable stub. HEAD adds offset-extraction scaffolding only. |

Source locations: `nativeunwind/elfunwindinfo/stackdeltaextraction.go:89-200`,
`interpreter/go/go.go:92-148`,
`nativeunwind/elfunwindinfo/elfgopclntab.go:235-485,654-840`,
`interpreter/hotspot/hotspot.go:6-153`,
`interpreter/python/python.go:39-71,783-1023`,
`interpreter/nodev8/v8.go:12-33,81-124,2200-2354`,
`interpreter/dotnet/dotnet.go:28-195`,
`interpreter/ruby/ruby.go:99-160,1463-1601`,
`interpreter/php/php.go:32-49,256-370`,
`interpreter/perl/perl.go:35-113`,
`interpreter/beam/beam.go:6-38,132-388`, and
`interpreter/luajit/luajit.go:30-56`.

Post-tag changes include ARM64 Go/CGo stack-switch recovery, corrected x86
register-held return addresses, CPython 3.14 stack-reference tagging, bounded
HotSpot/.NET readers, .NET dynamic-method names, and coarse Ruby YJIT handling.
The remaining matrix is materially unchanged.

## Concrete Go resource limits

These distinguish real caps from lifecycle cleanup and unbounded structures.
For map scale factor `s`, the supported range is zero through eight.

| Kernel structure | Bound/default |
| --- | --- |
| `per_cpu_records`, `per_cpu_records_kp` | One record per possible CPU; each value at most 32 KiB. |
| `metrics` | Per-CPU array, 117 slots at HEAD / 114 pinned. |
| `perf_progs`, `kprobe_progs` | 13 program slots. |
| `report_events` | Perf array and one-page reader buffer per possible CPU. |
| `reported_pids` | 65,536-entry LRU. |
| `pid_events` | 65,536-entry coalescing hash. |
| `pid_page_to_mapping_info` | No-preallocation LPM trie, `2^(20+s)` entries; 1,048,576 default, 268,435,456 maximum. |
| `inhibit_events` | Two entries. |
| `trace_events` | Next power of two of rate times CPU count times 25,304 bytes; capped at 2 GiB. |
| `traces_ctx_v1` | 16,384-entry LRU with OBI; one entry otherwise. |
| `apm_int_procs` | 128 entries. |
| `exe_id_to_*_stack_deltas` | Sixteen outer maps, each `2^(16+s)` files; inner buckets `2^8` through `2^23` deltas; over 8,388,608 per file rejected. |
| `unwind_info_array` | 16,384 unique lifetime rules; no individual reclamation. |
| `interpreter_offsets` | 32 interpreter ELF IDs, one/two ranges each. |
| `stack_delta_page_to_info` | `2^(16+s)` entries for 64-KiB code pages. |
| Runtime process maps | HotSpot/BEAM 256; Python/Perl/PHP/Ruby/V8/.NET/Go/LuaJIT 1,024 each. |
| `sched_times` | Off-CPU LRU-per-CPU hash, clamped from probability to 16-4,096 entries. |
| `ext_probe_value` | HEAD-only one-entry per-CPU array. |
| `system_analysis` | One startup-only entry, closed after initialization. |

Map evidence: `support/ebpf/interpreter_dispatcher.ebpf.c:14-148`,
`tracer/tracer.go:666-749`, `support/ebpf/native_stack_trace.ebpf.c:68-122`,
`processmanager/ebpf/ebpf.go:404-468`,
`processmanager/execinfomanager/manager.go:352-355,540-552`,
`probes/offcpu/offcpu.go:69-72,140-152`, and per-runtime map declarations.

Stack payload is 3,072 words (24 KiB, roughly 1,024 three-word frames), with an
error-frame slot reserved. Kernel stack helper capacity is 127 addresses.
The profiler stops at 29 tail calls. Native unwinding uses 16 binary-search
steps and five frames per invocation. Runtime invocation bounds are Python
10/15 (before/after kernel 6.6), Perl 12, PHP 19, Ruby 32, V8 8, .NET 6/9
(before/after .NET 10), BEAM 8, and HotSpot 4.

Go label capture allows 10 labels with 15-byte keys and 47-byte values; older Go
maps scan at most 16 buckets of eight slots. Excess labels and truncation are
silently omitted. See `support/ebpf/types.h:624-687`,
`support/ebpf/tracemgmt.h:536-600,866-890`, and runtime tracer constants.

| Userspace structure | Bound or absence of one |
| --- | --- |
| Frame LRU | 16,384 default; configurable 1,024-1,048,576. |
| ELF cache | 16,384 entries / six-hour TTL. |
| Failed stack-delta extraction | 8,192 entries / 90-second TTL. |
| Executable state | No count cap; unused data unloads after five minutes. |
| Process/interpreter/mapping/exit/probe state | No explicit userspace count cap; liveness-driven cleanup. |
| Async map updater | 16 workers with eight queued operations each; producers block. |
| PID channels | Capacity 10 for events, one for triggers; batch size 128. |
| Trace channel | Unbuffered, directly propagating downstream delay. |
| Ring processing | At most 4,096 records per 250-ms polling iteration. |
| Perf reader | One page per CPU; 100-ms read deadline. |
| Reporter aggregation | Unbounded resource/profile/sample maps and timestamp/value slices within an interval. |
| Failed export | Swapped interval is not requeued; loss instead of retention. |
| Standalone RPC | Five-second operation, three-second connection timeout, one-minute startup backoff, five retries, 32-MiB limit; no output splitting. |
| Metrics staging | 306 slots at HEAD / 303 pinned; one value per ID per second. |
| Common runtime LRUs | Usually 1,024; Perl COP 8,192; HotSpot symbols 2,048/stubs 128; V8 sources 128/map types 32. |
| .NET global caches | PE 16,384/6h, errors 1,024/6h, strings 1,024/1h. |
| Kernel/BPF symbols | No count cap; names truncate to 255 bytes. |
| Probe origin IDs | IDs 1-65,535, never reclaimed; HEAD uprobe PID/link maps are liveness-bound, not count-bound. |

Userspace evidence: `processmanager/manager.go:42-147`,
`processmanager/execinfomanager/manager.go:41-48,275-358`,
`processmanager/ebpf/asyncupdate.go:49-82`,
`tracer/events.go:27-43,97-143,173-255`,
`reporter/base_reporter.go:33-107`, `reporter/otlp_reporter.go:114-204`,
`metrics/metrics.go:20-39,138-192`, `kallsyms/kallsyms.go:57-80,159-190`,
and `tracer/tracer.go:1320-1349`.

Additional parser bounds are 128 MiB for Go pclntab/read-only data, 128 cached
CIEs, 8-KiB proc-maps/cgroup lines, and 64-KiB Process Context payloads with
three attempts. HEAD adds HotSpot stride/table/class/inline-scope/reader caps
and .NET 64-KiB structures / 1,048,576-node walks that were target-data-driven
at the pinned tag.

## Go telemetry and loss coverage

HEAD defines 306 metric slots with 231 active fields; pinned defines 303/228.
The additions are Go asmcgocall counters. Families include native/runtime BPF
attempts, frames and errors; generic stack/TLS/transport/PID failures; process
and map operations; occupancy; cache hit/miss/add/delete; and runtime
symbolization success/failure. There are no BEAM-specific BPF counters.

Catalog locations are `metrics/ids.go:14-195,203-414,416-708`,
`metrics/metrics.json`, and `support/types_def.go:213-325`. Some declared
kernel fallback-symbol LRU and non-batch map-update metrics are not wired;
Go label attempts/errors are incremented in BPF but absent from the translation
table.

| Loss path | Observation |
| --- | --- |
| Perf notification overflow | Exact `agent.errors.perf_event_lost`; empty/read errors separate. |
| Trace ring full | `bpf.errors.ringbuf_output`; no separate userspace ring-loss count. |
| Poll cutoff or blocked trace channel | No direct count; subsequent ring saturation may reveal pressure. |
| PID map updates | `bpf.errors.pid_events` / `reported_pids`. |
| Ordinary PID rate limit/coalescing | No drop count; priority deferral has a counter. |
| Idle/error-only filtering | No drop counter. |
| Minimum process age | `bpf.samples.skipped_process_too_new`. |
| Stack/tail-call exhaustion | `bpf.errors.stack_length_exceeded` / `bpf.tail_calls_max`. |
| Native/runtime unwind failures | Runtime counters and often error frames. |
| Ruby JIT native-resume truncation | Error frame, no dedicated metric. |
| Excess Go labels/buckets/truncation | Silent, except invalid UTF-8 drops. |
| Off-CPU LRU/update/missing timestamp | No dedicated counter. |
| Probe-specific loss | No probe-specific metric. |
| Async outer-map update failure | Logged only. |
| Unknown origin/profile type | Warned/skipped without a counter. |
| Collector/OTLP export failure | Logged, no requeue, no export-drop/retry metric. |
| Reporter aggregation growth | Unbounded, without size/high-water/drop measurement. |

Loss evidence: `support/ebpf/tracemgmt.h:331-370,602-625`,
`tracer/events.go:229-242,291-321`, `support/ebpf/off_cpu.ebpf.c:45-104`,
`processmanager/ebpf/asyncupdate.go:109-121`,
`reporter/collector_reporter.go:83-106`, and
`reporter/otlp_reporter.go:114-173`.

These gaps motivate explicit sample-fate accounting, bounded reporting, and
separate timeout/metadata diagnostics in the independent Rust implementation.

## Explicitly rejected or deferred behavior

- Unbounded channels, reporter trees, raw vectors, and effectively unlimited
  unwind budgets: **Reject**.
- Hash-only sample identity: **Reject**.
- Runtime panics and detached worker ownership: **Reject**.
- Mandatory OTLP, pprof, Arrow, OTAP, Collector, Go, or async-runtime output:
  **Reject** for the reusable core.
- `.eh_frame`, DWARF, inline frames, managed runtimes, off-CPU, allocation
  profiling, executable upload, and trace correlation: **Defer**.
- Per-core userspace workers: **Defer** until measurement demonstrates value.
