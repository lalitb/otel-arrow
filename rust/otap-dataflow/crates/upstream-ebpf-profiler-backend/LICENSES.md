# Licensing and Provenance

This file is an engineering provenance record, not legal advice.

## Material used

No upstream source or compiled eBPF object is committed in this crate.

The implementation was informed by read-only inspection of:

- repository:
  `https://github.com/open-telemetry/opentelemetry-ebpf-profiler`;
- commit: `06ea040c39d3d17bc1534a5dcc044368caf48782`;
- `support/ebpf/*.ebpf.c`;
- `support/ebpf/*.h`;
- `support/ebpf/Makefile`;
- `support/support*.go`;
- `support/types*.go`;
- `tracer/`;
- `processmanager/`;
- `nativeunwind/`;
- `interpreter/`;
- `libpf/`;
- `metrics/`;
- `reporter/`.

The crate independently expresses the documented binary ABI in Rust and keeps a
machine-readable inventory of object metadata. It does not contain copied C,
Go, generated bindings, or object bytes.

The new `tests/c_abi_layout.c` is an independently written layout probe. An
opt-in test compiles it against the external upstream headers in a temporary
directory, runs it to print numeric sizes/offsets, and removes the executable.
Neither those GPL headers nor that temporary executable is distributed here.
This is a description of the boundary, not a legal conclusion about it.

## License boundary observed upstream

The upstream repository root `LICENSE` is Apache License 2.0. Go userspace
files carry:

```text
SPDX-License-Identifier: Apache-2.0
```

The eBPF source directory has its own `support/ebpf/LICENSE`, which is the GNU
General Public License version 2. The ELF artifact contains the BPF license
string `GPL`.

The two pinned compiled objects are:

| Artifact | SHA-256 |
| --- | --- |
| `tracer.ebpf.amd64` | `0f186e7f99d2b544fc69ec02834ebfb902e09e4ab89eda51436a4ad8a2ec7282` |
| `tracer.ebpf.arm64` | `200a60f6bc10f8b964fd2b8170750f90bb7514bf73761948467c275b11d0849b` |

The object includes code compiled from all upstream `*.ebpf.c` translation
units and shared headers. It therefore includes excluded interpreter, off-CPU,
probe, integration-test, and correlation code even when Rust does not load
those programs.

## Generated and vendored upstream inputs

`support/ebpf/errors.h` is generated from
`tools/errors-codegen/errors.json`. The object also contains declarations and
ABI types from upstream-maintained headers such as:

- `bpfdefs.h`;
- `errors.h`;
- `extmaps.h`;
- `frametypes.h`;
- `go_runtime.h`;
- `kernel.h`;
- `native_stack_trace.h`;
- `tracemgmt.h`;
- `tsd.h`;
- `types.h`;
- `v8_tracer.h`.

The upstream `LICENSES/` tree records licenses for vendored Go dependencies.
Those dependencies are not linked into or redistributed by this Rust crate.

Header fingerprints used by the independent C fixture on 2026-09-05:

| External header | SHA-256 |
| --- | --- |
| `support/ebpf/types.h` | `2891c12212fcf5e4cb7d1482b82edb8d9da2cc279f0c75651551515dab5dc974` |
| `support/ebpf/kernel.h` | `57881717a0e376ea4d3db8c9625a5e0722e57b7eb40b6aea92bbf1c081b31470` |
| `support/ebpf/errors.h` | `cee5da583c37ab74d2f23a5b676e8b269aa561b52d895b744259bf6559e665b2` |

## Current distribution behavior

The eBPF object path is external configuration. This repository does not:

- embed the object;
- copy the GPL source;
- build the object;
- download the object;
- package it in a binary or container;
- claim that the Apache-2.0 Rust crate changes the object's license.

The checked-in JSON manifest contains names, numeric ABI metadata, and hashes
derived from inspection. It does not contain executable instructions or source.

## In-memory native-probe preparation

The loader now creates an owned, in-memory copy with six ELF64 REL symbol
associations changed in `kprobe/unwind_native` and `kprobe/unwind_stop`:
`per_cpu_records` becomes `per_cpu_records_kp`, and `perf_progs` becomes
`kprobe_progs`. This is based on upstream `tracer/tracer.go` functions
`loadProbeUnwinders` and `progArrayReferences` at the pinned commit.

No instruction section, data section, BTF, or license bytes are changed. No
upstream source or file is modified, and no prepared object is written,
embedded, committed, or distributed. The `prepare` example prints only hashes
and the six metadata associations. Prepared hashes are recorded in
[COMPATIBILITY.md](COMPATIBILITY.md).

The initial equivalence spike ran in a disposable directory under `/tmp`,
with the upstream GPL license and exact provenance retained there. Its
non-privileged comparison was then added as a source test. This engineering
record is not a legal conclusion about the transformed in-memory object;
any future distribution must include this transformation in human review.

## Temporary analysis request-filter patch

Kernel-stack discovery uses the existing `read_task_struct` program in a
temporary object. Upstream filters its shared request by TGID, which permits
other threads in the same process to race that request. The Rust backend applies
a separate, explicitly pinned analysis-only instruction change:

```text
Section: raw_tracepoint/sys_enter
Instruction byte offset: 0xd8
Original: 61 a0 fc ff 00 00 00 00   (load namespace TGID at fp-4)
Prepared: 61 a0 f8 ff 00 00 00 00   (load namespace TID at fp-8)
```

Analysis always enables translation to the requesting thread's current PID
namespace and writes its local TID into the request field. The normal sampler's
PID mode is configured independently. Exactly one file byte changes in the
owned temporary input; all other instructions, data, BTF, and license bytes are
unchanged. No source or on-disk artifact in a reference repository is modified.

| Architecture | Analysis-only prepared SHA-256 |
| --- | --- |
| amd64 | `5f2ba2305429daa29c57f54f219928de743ff83af3629c805a35c6663b7ebaeb` |
| arm64 | `fc5bc74b29db811f0cbcffd1925d71417d363d4c1660b8368f42fa5c9131beb5` |

`prepare <object> analysis` reports this patch and its prerequisites without
writing a prepared object. This instruction change is distinct from the
instruction-preserving six-relocation sampler policy above. It must be included
in provenance and legal review before any future artifact distribution.

## Reproducible upstream build

At the pinned revision, the source build recipe is below. It was inspected,
not executed to reproduce either binary during this run. Use a separate
writable checkout or disposable source copy at the pinned commit, never the
read-only reference repository:

```bash
cd support/ebpf
make TARGET_ARCH=amd64
make TARGET_ARCH=arm64
```

It expects version 17 names for Clang, llvm-link, llc, llvm-strip, and
clang-format, plus Go for `errors.h` generation. The Makefile uses deterministic
source and macro prefix maps, the `__SOURCE_DATE_EPOCH__=0` preprocessor
definition, and deterministic archive stripping. A redistribution process must
record actual compiler/tool versions, reproduce the object from the exact
commit, and compare its SHA-256 with the pinned binary.

No upstream source or on-disk artifact modifications were made. Both in-memory
preparation policies are documented above. If future source or additional object
fixes are needed, preserve an isolated patch set, the original license, all
generated-header inputs, and build commands.
The corresponding-source strategy should deliver that exact source and patch
set with required notices, subject to human review. The repository's preferred
object distribution method has not been established by this experiment.

## Human legal-review questions

Before an object or source is distributed with an otel-arrow binary, obtain
human legal review for:

1. whether the compiled GPL-2.0 eBPF object is an independent work when loaded
   by this Apache-2.0 Rust userspace;
2. whether bundling the object in one package, image, executable resource, or
   installer changes that analysis;
3. the corresponding-source delivery method required for the exact object;
4. notices and attribution required in binary and container distributions;
5. whether generated `errors.h` and any copied UAPI declarations need
   additional notices;
6. whether object modifications, including making local program symbols global
   for libbpf, require prominent modification notices or a separate source
   patch distribution;
7. whether downloading an object at install or runtime creates different
   obligations from embedding it;
8. whether the compatibility manifest can be distributed independently of the
   GPL artifact.

Until that review is complete, keep the object external and do not add it to
the repository, crate package, release archive, or container image.

## Separate Go oracle provenance

A disposable Go reference binary was built outside this repository with a local
module replacement targeting the exact pinned source revision. Upstream's normal
`go:embed` path includes its existing amd64 eBPF object; byte comparison confirmed
the exact 894,280-byte artifact in that oracle binary.

The oracle is not in the Rust runtime path and is not distributed by this crate.
Its separate session-artifact directory preserves Apache-2.0 userspace and GPLv2
eBPF licenses, upstream dependency notices, the new harness source, final module
files, and build provenance. No upstream source or artifact was changed.

The toolchain was downloaded from the official Go distribution:

```text
go1.26.0.linux-amd64.tar.gz
SHA-256: aac1b08a0fb0c4e0a7c1555beb7b59180b05dfc5a3d62e40e9de90cd42f88235
```

The final timing-instrumented oracle's provenance is:

| Material | SHA-256 |
| --- | --- |
| Harness source | `c6f148e3fa263f23d62650fc0e4e2475eba38dd953ad744fdf90b966fcd03de6` |
| Harness go.mod | `87577e67301a12dc7aa1c10766f1dea3e3ae1a5b769d6b6366831f782b7d9f6a` |
| Harness go.sum | `8a835c3d278a6bc0db8a1db18d8fc9f9768318fb05f5284e80d680920ba60c53` |
| Go oracle binary | `570379ecfefb27aa98c674610d03eb3339e8bd90a9ad92fe387bd6a2c7fbd153` |
| Shared frame-pointer workload | `0b7174758e5af0ef5c03fbcd1f48b2886ba0207b8fd90b628fc89ea83d3474be` |

## Reference projects

Lightswitch and Profile Bee were inspected only as architectural references at
their pinned commits. No source from either project was copied. Their licensing
and dependency choices do not determine the licensing of this implementation.
