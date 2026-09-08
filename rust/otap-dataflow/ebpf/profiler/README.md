# Native profiler eBPF program

This directory retains the original GPL-2.0-only C kernel sampler used by the
Apache-2.0 Rust profiler crate. Clang builds it separately; ordinary Cargo checks
do not require Clang, BPF capabilities, or kernel headers. No reference-profiler
code or prebuilt binary is incorporated.

## Reproducible build and contract tests

```bash
bash rust/otap-dataflow/ebpf/profiler/build.sh
BPF_TARGET_ARCH=aarch64 CLANG=clang-18 \
  bash rust/otap-dataflow/ebpf/profiler/build.sh \
  rust/otap-dataflow/ebpf/profiler/target/aarch64/profiler.bpf.o
CLANG=clang-18 bash rust/otap-dataflow/ebpf/profiler/tests/contract.sh
```

`CLANG` selects a local installation without changing the system toolchain.
Clang needs the BPF backend. Both targets explicitly use **bpfel**, not the host's
endianness. Supported target names are `x86_64`/`amd64` and `aarch64`/`arm64`.
The sampler uses portable helpers, so the two target objects can be identical;
their manifests still declare the intended runtime architecture.

Reproducibility requires the same compiler executable/package and source bytes.
The tested compiler is Ubuntu Clang 18.1.8 (`1:18.1.8-20ubuntu8`). Builds remove
absolute source/compilation paths, set `SOURCE_DATE_EPOCH=0`, and record the full
compiler version plus executable digest. The source digest hashes, in order,
`src/profiler.bpf.c` followed by `src/abi.h`. Generated objects and manifests live
in the ignored `target/` directory; do not commit them.

The contract script builds both architectures twice, compares objects and
manifests, compiles an independent native C ABI fixture, and runs Rust tests.
It also executes the original kernel C with native mock helpers, compares the
emitted event to the fixture, and tests partial stack failures, zeroed padding,
idle filtering and output-loss counters without BPF capabilities.
Those tests inspect the generated ELF/map shapes and helper calls, including the
BOOTTIME helper; decode the C fixture; and exercise every perf ring split with
zero through seven padding bytes. The script also builds the smoke workload
with frame pointers. A native Rust harness additionally parses both objects
using the exact `aya-obj` artifact reported by Cargo, without making kernel
syscalls or changing dependencies. The script uses existing `cc`, Clang, Cargo,
Rust and Python installations.

Pass the object through `ProfilerConfig::program.object_path` or
`OTEL_EBPF_PROFILER_OBJECT`. The bounded manifest declares the ABI, architecture,
clock, endianness, names and digests. Duplicate keys are rejected. The loader
opens a bounded regular file once and uses those same bytes for hashing, layout
validation and Aya loading. A digest detects corruption or accidental mismatch:
**it provides integrity, not authenticity**. An attacker who can replace both
the object and manifest can replace the program. Provision them from a trusted
source with appropriate filesystem permissions.

## Kernel contract

The ABI is independently represented in `src/abi.h` and Rust `event.rs`, with
compile-time size, offset and alignment assertions. It has a 48-byte header,
64 user frames and 64 kernel frames, totaling 1,072 bytes aligned to eight.
Reserved fields and unused frames must be zero. Negative helper errors require
the corresponding flag and zero depth; truncation flags mean the 64-frame
capacity was filled (not proof that another frame existed).

Aya exposes perf transport padding after the ABI payload. Userspace removes
only that bounded framing before exact-size ABI validation, including records
that wrap the ring, using a fixed-size copy rather than allocating a buffer.
Each drain consumes a bounded count of records, including malformed and lost
records, rotates fairly across CPUs, and leaves unread events buffered.

`SCRATCH` is one 1,072-byte per-CPU value, since the complete event cannot fit on
the 512-byte BPF stack. Every unused frame is cleared after stack collection,
including partial helper failures. `COUNTERS` has three per-CPU `u64` slots:
sampling periods attempted, output-helper failures, and intentionally skipped
idle tasks. Stop-time statistics sum possible-CPU values with saturating
userspace arithmetic. Perf lost notifications are a separate diagnostic and can
overlap output failures: **never sum those two as independent kernel loss**.
The coordinator reconciles non-idle attempts against received records.

The `EVENTS` source map has 4,096 entries. Aya overrides its loaded size to
`max_cpu_id + 1` for sparse CPU IDs. Possible CPU count is bounded independently
of selected monitoring count before per-CPU maps are allocated. Buffer sizes
must be an exact power-of-two number of native pages, plus Aya's metadata page.
Preparation loads programs/maps and opens buffers without enabling sampling.
Only `start_sampling`, after worker initialization, attaches CPU-clock triggers.
Best-effort CPU failures are counted; strict failures release partial ownership.

## Runtime prerequisites and privileged smoke

Runtime support requires Linux **5.8 or newer**, little-endian x86_64/aarch64,
and effective **CAP_BPF plus CAP_PERFMON**, or **CAP_SYS_ADMIN**. UID zero alone
is insufficient. Events use `bpf_ktime_get_boot_ns`: timestamps are
**CLOCK_BOOTTIME**, including suspend, and can be compared with procfs process
start ticks. Native user collection guarantees frame-pointer unwinding only,
not DWARF/interpreter/JIT unwinding.

There are no CO-RE relocations or required kernel BTF types; deployment policy
may optionally require `/sys/kernel/btf/vmlinux`. Perf policy, seccomp, LSMs,
lockdown, memory limits or kernel configuration can still deny loading.
Aya's own loader performs kernel feature/BTF discovery internally; the crate
does not vendor or replace Aya's implementation.

### Aya initialization allocations

Normal verifier logging is explicitly disabled. Aya 0.14 still allocates a
10,240-byte retry buffer after the first failed BTF/program load, and retries
`ENOSPC` with a logical log limit of 16,777,215 bytes. The current Rust `Vec`
growth policy can retain 20,480,000 bytes for that buffer; old and new allocations
during growth can require 30,720,000 bytes together. Reserve at least 32 MiB of
initialization/error scratch if budgeting this failure path. Truncating the
returned diagnostic to 4 KiB does not prevent those earlier allocations.

`EbpfLoader::new()` eagerly reads and parses the running kernel's BTF before
`.btf(None)` can disable its use. Aya exposes no byte/count ceiling or lazy
constructor for that read. Before constructing Aya, the profiler now bounds and
validates the immutable kernel sysfs export: 8 MiB, 262,144 type records and
524,288 aggregate members/parameters/enum values by default. Mutable non-sysfs
substitutes are rejected. Unknown or malformed type layouts are rejected before
Aya can allocate from them.

The initialization envelope reserves vector growth and bookkeeping for those
caps, plus the 32 MiB verifier retry allowance and object-parser scratch.
The native contract harness checks the selected Aya artifact's type/member
sizes against the allocation-model assumptions. The default object limit is
256 KiB; preflight also bounds symbol names, section counts, map shapes and
BTF extension metadata, and rejects CO-RE records for this helper-only program.

Initialization finishes before workers start, so the total requested envelope
is the maximum of initialization and running storage, not their sum. The
default initialization bound is 249,815,040 bytes (about 238 MiB); running
storage is 244,069,824 bytes. Both fit the configured 256 MiB ceiling.
This does not promise exact RSS or defend against an administrator changing
the process's mount namespace during initialization. No mount or namespace is
changed by the profiler.

The kernel-BTF deployment preflight can be exercised without BPF privileges:

```bash
OTEL_EBPF_PROFILER_CHECK_BTF=1 \
  cargo test -p otel-arrow-dfe-ebpf-profiler --lib linux::btf_preflight
```

Before Aya sees the ELF, preflight permits exactly three fixed-shape maps,
rejects implicit/global-data maps, and validates key/value sizes, flags and entry
counts. `EVENTS` is then explicitly resized to the configured CPU-ID bound.
Steady-state descriptors are at most two per selected CPU (output ring and
sampling trigger/link), three map descriptors, one loaded program descriptor
and one optional BTF descriptor. Initialization feature probes briefly acquire
additional descriptors. Per-CPU map storage depends on possible CPU count, not
the number of descriptors or selected CPUs.

```bash
cd rust/otap-dataflow
OTEL_EBPF_PROFILER_SMOKE=1 \
OTEL_EBPF_PROFILER_OBJECT="$PWD/ebpf/profiler/target/x86_64/profiler.bpf.o" \
  cargo test -p otel-arrow-dfe-ebpf-profiler --test real_kernel -- --nocapture
```

The opt-in smoke skips only verified missing architecture/kernel/capability
prerequisites. On a capable host, object, verifier, map, attachment and runtime
errors fail. It uses `cc` and `taskset` to start a deterministic, pinned,
frame-pointer-enabled C child, then requires that **child PID's nonempty user
stack**, reconciled accounting, idempotent shutdown, another successful startup
using the user-plus-kernel program, and no leaked FDs. RAII cleanup releases the
child and profiler even on assertion failure. Neither tests nor scripts change
capabilities, sysctls, or privileges.

An isolated guest need not contain a compiler. Set
`OTEL_EBPF_PROFILER_WORKLOAD` to the prebuilt `native-workload` emitted by
`tests/contract.sh`, and copy the object, manifest, prebuilt test executable,
`taskset`, and their shared-library dependencies into the guest. Use only a
trusted workload built with the documented frame-pointer flags. The test still
requires the child-owned user stack and cleanup checks; providing this path
does not bypass kernel or capability prerequisites.
