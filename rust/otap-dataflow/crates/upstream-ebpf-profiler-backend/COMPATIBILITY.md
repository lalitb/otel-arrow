# Upstream Compatibility Contract

This document describes the object at upstream commit
`06ea040c39d3d17bc1534a5dcc044368caf48782`. The machine-readable source of
truth is
`compatibility/upstream-06ea040c-amd64.json`, with a separately pinned arm64
inventory in `compatibility/upstream-06ea040c-arm64.json`.

## Artifact fingerprints

| Architecture | File size | SHA-256 |
| --- | ---: | --- |
| amd64 | 894,280 | `0f186e7f99d2b544fc69ec02834ebfb902e09e4ab89eda51436a4ad8a2ec7282` |
| arm64 | 915,320 | `200a60f6bc10f8b964fd2b8170750f90bb7514bf73761948467c275b11d0849b` |

Both are ELF64 little-endian relocatable eBPF objects, not stripped of BTF.
Both contain 38 programs, 47 maps, 23 `.rodata.var` globals, `.BTF`, and
`.BTF.ext`.

The e_machine value is BPF for both architectures. Architecture is detected
from architecture-specific `pt_regs` BTF signatures. This is necessary because
the ELF header alone does not distinguish amd64-targeted and arm64-targeted BPF.

## Programs

The instruction counts are pre-relocation counts from the pinned amd64 object.

| Program | Section | Type | Instructions |
| --- | --- | --- | ---: |
| `finish_task_switch` | `kprobe/finish_task_switch` | kprobe | 961 |
| `kprobe__dummy` | `kprobe/dummy` | kprobe | 16 |
| `kprobe__external` | `kprobe/external` | kprobe | 949 |
| `kprobe__generic` | `kprobe/generic` | kprobe | 935 |
| `kprobe_go_labels` | `kprobe/go_labels` | kprobe | 415 |
| `kprobe_unwind_beam` | `kprobe/unwind_beam` | kprobe | 674 |
| `kprobe_unwind_dotnet` | `kprobe/unwind_dotnet` | kprobe | 926 |
| `kprobe_unwind_dotnet10` | `kprobe/unwind_dotnet10` | kprobe | 889 |
| `kprobe_unwind_hotspot` | `kprobe/unwind_hotspot` | kprobe | 1,223 |
| `kprobe_unwind_luajit` | `kprobe/unwind_luajit` | kprobe | 74 |
| `kprobe_unwind_native` | `kprobe/unwind_native` | kprobe | 988 |
| `kprobe_unwind_perl` | `kprobe/unwind_perl` | kprobe | 975 |
| `kprobe_unwind_php` | `kprobe/unwind_php` | kprobe | 547 |
| `kprobe_unwind_python` | `kprobe/unwind_python` | kprobe | 1,821 |
| `kprobe_unwind_ruby` | `kprobe/unwind_ruby` | kprobe | 1,130 |
| `kprobe_unwind_stop` | `kprobe/unwind_stop` | kprobe | 1,107 |
| `kprobe_unwind_v8` | `kprobe/unwind_v8` | kprobe | 1,024 |
| `native_tracer_entry` | `perf_event/native_tracer_entry` | perf event | 935 |
| `perf_go_labels` | `perf_event/go_labels` | perf event | 415 |
| `perf_unwind_beam` | `perf_event/unwind_beam` | perf event | 674 |
| `perf_unwind_dotnet` | `perf_event/unwind_dotnet` | perf event | 926 |
| `perf_unwind_dotnet10` | `perf_event/unwind_dotnet10` | perf event | 889 |
| `perf_unwind_hotspot` | `perf_event/unwind_hotspot` | perf event | 1,223 |
| `perf_unwind_luajit` | `perf_event/unwind_luajit` | perf event | 74 |
| `perf_unwind_native` | `perf_event/unwind_native` | perf event | 988 |
| `perf_unwind_perl` | `perf_event/unwind_perl` | perf event | 975 |
| `perf_unwind_php` | `perf_event/unwind_php` | perf event | 547 |
| `perf_unwind_python` | `perf_event/unwind_python` | perf event | 1,821 |
| `perf_unwind_ruby` | `perf_event/unwind_ruby` | perf event | 1,130 |
| `perf_unwind_stop` | `perf_event/unwind_stop` | perf event | 1,107 |
| `perf_unwind_v8` | `perf_event/unwind_v8` | perf event | 1,024 |
| `read_kernel_memory` | `tracepoint/syscalls/sys_enter_bpf` | tracepoint | 60 |
| `read_task_struct` | `raw_tracepoint/sys_enter` | raw tracepoint | 65 |
| `tracepoint__sched_process_free` | `tracepoint/sched/sched_process_free/v2` | tracepoint | 174 |
| `tracepoint__sched_process_free_pre616` | `tracepoint/sched/sched_process_free/v1` | tracepoint | 174 |
| `tracepoint__sched_switch` | `tracepoint/sched/sched_switch` | tracepoint | 136 |
| `tracepoint__sys_exit_prctl` | `tracepoint/syscalls/sys_exit_prctl` | tracepoint | 364 |
| `tracepoint_integration__sched_switch` | `tracepoint/integration/sched_switch` | tracepoint | 204 |

### Native-only loaded programs

The prototype selects six programs, now loaded successfully on the tested
Linux amd64 kernel:

1. `native_tracer_entry`;
2. `perf_unwind_native`;
3. `perf_unwind_stop`;
4. `kprobe_unwind_native`;
5. `kprobe_unwind_stop`;
6. exactly one `sched_process_free` layout selected at Linux 6.16.

Every other program remains parsed but is not selected for loading. The two
native probe targets initialize the second chain but are not attached to a
custom probe. The actual perf-only dependency minimum remains four programs.

## Tail-call topology

`perf_progs` and `kprobe_progs` both have 13 entries. The source enum is:

| Index | Logical program | Perf copy | Probe copy | Native-only |
| ---: | --- | --- | --- | --- |
| 0 | `unwind_stop` | `perf_unwind_stop` | `kprobe_unwind_stop` | Yes |
| 1 | `unwind_native` | `perf_unwind_native` | `kprobe_unwind_native` | Yes |
| 2 | `unwind_hotspot` | `perf_unwind_hotspot` | `kprobe_unwind_hotspot` | No |
| 3 | `unwind_perl` | `perf_unwind_perl` | `kprobe_unwind_perl` | No |
| 4 | `unwind_python` | `perf_unwind_python` | `kprobe_unwind_python` | No |
| 5 | `unwind_php` | `perf_unwind_php` | `kprobe_unwind_php` | No |
| 6 | `unwind_ruby` | `perf_unwind_ruby` | `kprobe_unwind_ruby` | No |
| 7 | `unwind_v8` | `perf_unwind_v8` | `kprobe_unwind_v8` | No |
| 8 | `unwind_dotnet` | `perf_unwind_dotnet` | `kprobe_unwind_dotnet` | No |
| 9 | `unwind_dotnet10` | `perf_unwind_dotnet10` | `kprobe_unwind_dotnet10` | No |
| 10 | `go_labels` | `perf_go_labels` | `kprobe_go_labels` | No |
| 11 | `unwind_beam` | `perf_unwind_beam` | `kprobe_unwind_beam` | No |
| 12 | `unwind_luajit` | `perf_unwind_luajit` | `kprobe_unwind_luajit` | No |

`tail_call()` always names `perf_progs` in the compiled source. The upstream Go
loader rewrites the kprobe copies to `kprobe_progs`. It also rewrites
`per_cpu_records` references to `per_cpu_records_kp`. This is why disabling
managed runtimes does not by itself make the probe chain usable from Aya.

The native-only implementation initializes slots 0 and 1 in both arrays and
retains both handles. Before Aya loading, `PreparedObject` redirects exactly
six ELF64 REL symbol references in the two native probe targets. Both scratch
loads become `per_cpu_records_kp`, and each tail-array load becomes
`kprobe_progs`. No perf-program relocation changes.

The prepared in-memory hashes are separately pinned:

| Architecture | SHA-256 after native probe preparation |
| --- | --- |
| amd64 | `0e1da58748c2e6df2a933bd3c7d61bc8b4a8c2edcc3db1ff2ad45641b72e4b2e` |
| arm64 | `23105184886581a1dbd8309f8f7c4cfaecb5d359995bb1f501aadbdf3057529a` |

All instruction, data, BTF, license, and other relocation sections remain
byte-identical. The `prepare` example prints metadata only. The original
external artifacts and their inventory manifests are not replaced.

Temporary task analysis uses a distinct, instruction-changing preparation
policy to filter by the requesting TID instead of its TGID. Its exact one-byte
change, hashes, and required namespace settings are in
[LICENSES.md](LICENSES.md). It is not applied to the sampling object.

## Maps

Map type numbers follow Linux UAPI. Key and value columns are byte sizes.
`0x1` is `BPF_F_NO_PREALLOC`; `0x80` is read-only to the BPF program for data
sections.

| Map | Type | Key | Value | Default max | Flags | Inner template |
| --- | --- | ---: | ---: | ---: | --- | --- |
| `.rodata` | array | 4 | 16,778 | 1 | `0x80` | - |
| `.rodata.var` | array | 4 | 108 | 1 | `0x80` | - |
| `apm_int_procs` | hash | 4 | 8 | 128 | `0x0` | - |
| `beam_procs` | hash | 4 | 40 | 256 | `0x0` | - |
| `dotnet_procs` | hash | 4 | 4 | 1,024 | `0x0` | - |
| `exe_id_to_8_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 256 |
| `exe_id_to_9_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 512 |
| `exe_id_to_10_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 1,024 |
| `exe_id_to_11_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 2,048 |
| `exe_id_to_12_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 4,096 |
| `exe_id_to_13_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 8,192 |
| `exe_id_to_14_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 16,384 |
| `exe_id_to_15_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 32,768 |
| `exe_id_to_16_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 65,536 |
| `exe_id_to_17_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 131,072 |
| `exe_id_to_18_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 262,144 |
| `exe_id_to_19_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 524,288 |
| `exe_id_to_20_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 1,048,576 |
| `exe_id_to_21_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 2,097,152 |
| `exe_id_to_22_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 4,194,304 |
| `exe_id_to_23_stack_deltas` | hash of maps | 8 | 4 | 4,096 | `0x0` | array, 4/4, 8,388,608 |
| `ext_probe_value` | per-CPU array | 4 | 8 | 1 | `0x0` | - |
| `go_procs` | hash | 4 | 36 | 1,024 | `0x0` | - |
| `hotspot_procs` | hash | 4 | 40 | 256 | `0x0` | - |
| `inhibit_events` | hash | 4 | 1 | 2 | `0x0` | - |
| `interpreter_offsets` | hash | 8 | 40 | 32 | `0x0` | - |
| `kprobe_progs` | program array | 4 | 4 | 13 | `0x0` | - |
| `luajit_procs` | hash | 4 | 1 | 1,024 | `0x0` | - |
| `metrics` | per-CPU array | 4 | 8 | 118 | `0x0` | - |
| `per_cpu_records` | per-CPU array | 4 | 26,640 | 1 | `0x0` | - |
| `per_cpu_records_kp` | per-CPU array | 4 | 26,640 | 1 | `0x0` | - |
| `perf_progs` | program array | 4 | 4 | 13 | `0x0` | - |
| `perl_procs` | hash | 4 | 40 | 1,024 | `0x0` | - |
| `php_procs` | hash | 4 | 24 | 1,024 | `0x0` | - |
| `pid_events` | hash | 8 | 1 | 65,536 | `0x0` | - |
| `pid_page_to_mapping_info` | LPM trie | 16 | 16 | 524,288 | `0x1` | - |
| `py_procs` | hash | 4 | 40 | 1,024 | `0x0` | - |
| `report_events` | perf-event array | 4 | 4 | 0 | `0x0` | - |
| `reported_pids` | LRU hash | 4 | 8 | 65,536 | `0x0` | - |
| `ruby_procs` | hash | 4 | 96 | 1,024 | `0x0` | - |
| `sched_times` | LRU per-CPU hash | 8 | 8 | 256 | `0x0` | - |
| `stack_delta_page_to_info` | hash | 16 | 8 | 40,000 | `0x0` | - |
| `system_analysis` | array | 4 | 144 | 1 | `0x0` | - |
| `trace_events` | ring buffer | 0 | 0 | 0 | `0x0` | - |
| `traces_ctx_v1` | LRU hash | 8 | 24 | 16,384 | `0x0` | - |
| `unwind_info_array` | array | 4 | 12 | 16,384 | `0x0` | - |
| `v8_procs` | hash | 4 | 28 | 1,024 | `0x0` | - |

### Map roles and ownership

| Group | Producer | Consumer | Native-only treatment |
| --- | --- | --- | --- |
| `.rodata*` | userspace loader | every loaded program | Required |
| `per_cpu_records` | entry and unwinders | entry and unwinders | Required |
| `per_cpu_records_kp` | probe unwinders | probe unwinders | Required by the initialized native probe chain |
| `metrics` | kernel programs | userspace | Required for ring-loss accounting |
| `perf_progs` | userspace | perf unwinders | Required, slots 0 and 1 |
| `kprobe_progs` | userspace | probe unwinders | Required, slots 0 and 1 |
| `report_events` | kernel | userspace perf buffers | Required |
| `pid_events` | kernel | userspace | Required |
| `reported_pids` | kernel and userspace | kernel | Required |
| `inhibit_events` | kernel and userspace | kernel | Required |
| `trace_events` | kernel | userspace ring reader | Required |
| `pid_page_to_mapping_info` | userspace | native unwinder | Required |
| `stack_delta_page_to_info` | userspace | native unwinder | Required |
| `exe_id_to_*` | userspace | native unwinder | Required map-in-map buckets |
| `unwind_info_array` | userspace | native unwinder | Required by program ABI |
| `interpreter_offsets` | userspace | native dispatcher | Minimized, empty |
| `go_procs` | userspace | native entry and labels | Minimized, empty |
| `apm_int_procs` | userspace | stop unwinder | Minimized, empty |
| `traces_ctx_v1` | OBI or userspace | stop unwinder | Minimized, empty |
| other interpreter maps | upstream Go userspace | interpreter programs | Minimized, unused |
| `sched_times` | off-CPU program | off-CPU program | Minimized, unused |
| `system_analysis` | userspace and analysis program | userspace | Used in a temporary object for bounded stack-layout discovery |
| `ext_probe_value` | custom probe | external trampoline | Minimized, unused |

The Go loader changes capacities before creation:

- `pid_page_to_mapping_info`: `2^(20 + map_scale_factor)`;
- `stack_delta_page_to_info`: `2^(16 + map_scale_factor)`;
- every outer stack-delta map: `2^(16 + map_scale_factor)`;
- `trace_events`: next power of two covering one second of worst-case traces,
  capped at 2 GiB;
- `report_events`: possible CPU count.

The Rust spike replaces these implicit formulas with explicit configuration
limits.

On the measured kernel, Go also applies `BPF_F_NO_PREALLOC` to the outer
stack-delta maps. Its observed 65,536-slot outer maps and 65,536-entry stack-page
map differ from Rust's 4,096-slot preallocated outer maps and 1,048,576-entry
stack-page map. These are configuration differences, not inherent Go/Rust
memory costs.

Dynamic inner maps use typed Aya array creation and map-in-map insertion.
`perf_progs` remains owned until ingress has stopped and the runtime is released.
The largest original inner-map template is still created transiently by Aya;
resizing an outer map does not reduce its inner template's capacity.

## Globals

Offsets and initial bytes are from `.rodata.var` in the pinned amd64 object.

| Global | Offset | Size | Initial bytes |
| --- | ---: | ---: | --- |
| `origin_id_probe` | 0 | 2 | `0000` |
| `filter_error_frames` | 2 | 1 | `00` |
| `go_labels_disabled` | 3 | 1 | `01` |
| `filter_idle_frames` | 4 | 1 | `00` |
| `inverse_pac_mask` | 8 | 8 | `0000000000000000` |
| `vma_lookup_enabled` | 16 | 1 | `00` |
| `vma_vm_file_offset` | 20 | 4 | `00000000` |
| `vma_vm_flags_offset` | 24 | 4 | `00000000` |
| `origin_id_sampling` | 28 | 2 | `0000` |
| `off_cpu_threshold` | 32 | 4 | `00000000` |
| `origin_id_off_cpu` | 36 | 2 | `0000` |
| `filter_min_process_age_ns` | 40 | 8 | `0000000000000000` |
| `task_group_leader_offset` | 48 | 4 | `00000000` |
| `task_start_time_offset` | 52 | 4 | `00000000` |
| `task_stack_offset` | 56 | 4 | `00000000` |
| `stack_ptregs_offset` | 60 | 4 | `00000000` |
| `python_frames_per_program` | 64 | 4 | `0a000000` |
| `ruby_skip_native_resume` | 68 | 1 | `00` |
| `tpbase_offset` | 72 | 8 | `0000000000000000` |
| `pid_ns_translation_enabled` | 80 | 1 | `00` |
| `target_pid_ns_dev` | 88 | 8 | `0000000000000000` |
| `target_pid_ns_inode` | 96 | 8 | `0000000000000000` |
| `with_debug_output` | 104 | 4 | `00000000` |

### Global sources in the Go loader

- operator configuration supplies debug, idle/error filters, process-age
  filter, PID namespace mode, Ruby behavior, Go labels, and origin IDs;
- kernel BTF supplies `task_struct.stack`, thread-pointer base,
  `group_leader`, `start_time`, and VMA field offsets;
- attached system-analysis programs determine `stack_ptregs_offset` and provide
  a BTF fallback;
- kernel helper probes decide `vma_lookup_enabled`;
- arm64 PAC probing supplies `inverse_pac_mask`.

The Rust native milestone discovers `task_stack_offset` and
`stack_ptregs_offset` through bounded, thread-scoped task analysis. It leaves
unused VMA/process-age/TLS offsets zero, disables VMA lookup, process-age
filtering, interpreters, labels, off-CPU and probe origins, and uses origin 1
for CPU sampling.

## Event ABI

The Rust layout assertions match upstream generated Go sizes. An independent,
explicitly selected C fixture also checks event and map layouts directly from
the external pinned headers:

| Type or field | Value |
| --- | ---: |
| `sizeof(Trace)` | 25,304 |
| `offsetof(Trace.frame_data)` | 728 |
| Maximum frame words | 3,072 |
| `sizeof(StackDelta)` | 4 |
| `sizeof(UnwindInfo)` | 12 |
| `sizeof(PIDPage)` | 16 |
| `sizeof(PIDPageMappingInfo)` | 16 |
| `sizeof(StackDeltaPageKey)` | 16 |
| `sizeof(StackDeltaPageInfo)` | 8 |

Kernel addresses are raw 64-bit words at the start of `frame_data`. User frames
follow. A frame header uses:

```text
bits 63..60: frame type
bits 59..56: frame flags
bits 55..52: total frame words, including the header
bits 51..0 : type-specific data
```

A native frame has two words:

1. header data: executable-relative address;
2. variable 0: 64-bit executable ID.

Unknown frame types and flag bits are retained. Zero length, truncated payload,
oversized count, excess kernel count, and header-count mismatch are rejected.

## System probing contract

The upstream userspace performs:

- Linux version parsing;
- kernel symbol discovery;
- BTF resolution of task and VMA fields;
- dynamic stack-register offset analysis;
- VMA helper availability probes;
- old-kernel `maccess` bug checking;
- no-preallocation feature probing;
- PID namespace device and inode lookup;
- memlock adjustment.

The Rust spike records kernel release, host architecture, possible and online
CPUs, capability bits, BTF readability, and the unprivileged-BPF sysctl. It now
uses the bounded upstream task-scan approach to discover kernel-stack layout.
Kernel-symbol analysis, legacy helper-call rewriting, and old-kernel bug probes
remain outside the supported Linux 5.17+ path.

The configured BTF path is used for Aya relocations, with an explicit byte
limit, including the default BTF input probed internally by Aya. Discovered
task/register offsets now support user-register recovery from kernel-mode
interruptions on the tested kernel.

The pinned `send_trace` uses `BPF_RB_NO_WAKEUP`; a ring reader cannot wait
indefinitely for readiness notifications. The Rust timer is configurable and
bounded by sampling rate, ring capacity, and per-drain capacity.

## Compatibility maintenance procedure

For each proposed upstream update:

1. build or obtain both architecture artifacts from a pinned commit;
2. record exact SHA-256 and file size;
3. run the `inspect` example for amd64 and arm64;
4. diff programs, sections, instruction counts, maps, inner templates, globals,
   and tail-call indexes;
5. inspect C and Go changes affecting runtime map rewriting or globals;
6. rerun non-privileged ABI and compatibility tests;
7. run authorized privileged object-load, one-CPU, multi-CPU, event, stack,
   rollback, and shutdown tests;
8. run the Go/Rust comparison workload;
9. create a new compatibility label rather than silently changing the existing
   manifest;
10. complete licensing and distribution review before shipping an object.
