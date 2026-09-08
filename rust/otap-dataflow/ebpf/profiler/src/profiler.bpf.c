// SPDX-License-Identifier: GPL-2.0-only
//
// Original OpenTelemetry profiler sampling program. No reference-profiler
// source or generated object is incorporated here.

#include "abi.h"

#define SEC(name) __attribute__((section(name), used))
#define BPF_MAP_TYPE_PERF_EVENT_ARRAY 4
#define BPF_MAP_TYPE_PERCPU_ARRAY 6
#define BPF_F_CURRENT_CPU 0xffffffffULL
#define BPF_F_USER_STACK (1ULL << 8)

struct bpf_map_def {
    __u32 type;
    __u32 key_size;
    __u32 value_size;
    __u32 max_entries;
    __u32 map_flags;
};

struct bpf_map_def SEC("maps") EVENTS = {
    .type = BPF_MAP_TYPE_PERF_EVENT_ARRAY,
    .key_size = sizeof(__u32),
    .value_size = sizeof(__u32),
    .max_entries = 4096,
};

struct bpf_map_def SEC("maps") SCRATCH = {
    .type = BPF_MAP_TYPE_PERCPU_ARRAY,
    .key_size = sizeof(__u32),
    .value_size = sizeof(struct profiler_event),
    .max_entries = 1,
};

// Three fixed slots, each a per-CPU u64: attempts, output failures, idle skips.
struct bpf_map_def SEC("maps") COUNTERS = {
    .type = BPF_MAP_TYPE_PERCPU_ARRAY,
    .key_size = sizeof(__u32),
    .value_size = sizeof(__u64),
    .max_entries = 3,
};

static void *(*bpf_map_lookup_elem)(void *map, const void *key) = (void *)1;
static __u64 (*bpf_get_current_pid_tgid)(void) = (void *)14;
static __u64 (*bpf_ktime_get_boot_ns)(void) = (void *)125;
static __u32 (*bpf_get_smp_processor_id)(void) = (void *)8;
static long (*bpf_get_stack)(void *ctx, void *buf, __u32 size, __u64 flags) =
    (void *)67;
static long (*bpf_perf_event_output)(void *ctx, void *map, __u64 flags,
                                     void *data, __u64 size) = (void *)25;

static __attribute__((always_inline)) inline void count(__u32 key) {
    __u64 *value = bpf_map_lookup_elem(&COUNTERS, &key);
    if (value)
        __sync_fetch_and_add(value, 1);
}

static __attribute__((always_inline)) inline int collect(void *ctx,
                                                        int include_kernel) {
    __u32 key = 0;
    struct profiler_event *event = bpf_map_lookup_elem(&SCRATCH, &key);
    __u64 pid_tgid = bpf_get_current_pid_tgid();
    long user_bytes;
    long kernel_bytes;

    count(0);
    if ((__u32)pid_tgid == 0) {
        count(2);
        return 0;
    }
    if (!event)
        return 0;

    event->version = ABI_VERSION;
    event->header_size = ABI_HEADER_SIZE;
    event->event_kind = EVENT_KIND_SAMPLE;
    event->flags = 0;
    event->pid = pid_tgid >> 32;
    event->tid = (__u32)pid_tgid;
    event->cpu = bpf_get_smp_processor_id();
    event->reserved0 = 0;
    event->timestamp_ns = bpf_ktime_get_boot_ns();
    event->user_depth = 0;
    event->kernel_depth = 0;
    event->user_stack_error = 0;
    event->kernel_stack_error = 0;
    event->reserved1 = 0;

    user_bytes = bpf_get_stack(ctx, event->frames,
                               MAX_STACK_DEPTH * sizeof(__u64),
                               BPF_F_USER_STACK);
    if (user_bytes < 0) {
        event->flags |= FLAG_USER_STACK_ERROR;
        event->user_stack_error = (__s32)user_bytes;
    } else {
        if (user_bytes > MAX_STACK_DEPTH * sizeof(__u64))
            user_bytes = MAX_STACK_DEPTH * sizeof(__u64);
        event->user_depth = (__u16)(user_bytes / sizeof(__u64));
        if (event->user_depth == MAX_STACK_DEPTH)
            event->flags |= FLAG_USER_TRUNCATED;
    }

    if (include_kernel) {
        kernel_bytes = bpf_get_stack(
            ctx, &event->frames[MAX_STACK_DEPTH],
            MAX_STACK_DEPTH * sizeof(__u64), 0);
        if (kernel_bytes < 0) {
            event->flags |= FLAG_KERNEL_STACK_ERROR;
            event->kernel_stack_error = (__s32)kernel_bytes;
        } else {
            if (kernel_bytes > MAX_STACK_DEPTH * sizeof(__u64))
                kernel_bytes = MAX_STACK_DEPTH * sizeof(__u64);
            event->kernel_depth = (__u16)(kernel_bytes / sizeof(__u64));
            if (event->kernel_depth == MAX_STACK_DEPTH)
                event->flags |= FLAG_KERNEL_TRUNCATED;
        }
    }

    // Helpers may partially write a buffer before returning an error.
#pragma clang loop unroll(full)
    for (int index = 0; index < MAX_STACK_DEPTH; index++) {
        if (index >= event->user_depth)
            event->frames[index] = 0;
        if (index >= event->kernel_depth)
            event->frames[MAX_STACK_DEPTH + index] = 0;
    }
    if (bpf_perf_event_output(ctx, &EVENTS, BPF_F_CURRENT_CPU, event,
                             sizeof(*event)) < 0)
        count(1);
    return 0;
}

SEC("perf_event/profile_cpu")
int profile_cpu(void *ctx) {
    return collect(ctx, 0);
}

SEC("perf_event/profile_cpu_kernel")
int profile_cpu_kernel(void *ctx) {
    return collect(ctx, 1);
}

char LICENSE[] SEC("license") = "GPL";
