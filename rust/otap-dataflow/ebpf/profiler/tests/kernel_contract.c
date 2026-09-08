// SPDX-License-Identifier: GPL-2.0-only

#include "../src/profiler.bpf.c"
#include <assert.h>
#include <stdio.h>
#include <string.h>

static struct profiler_event scratch;
static struct profiler_event emitted;
static __u64 counters[3];
static int output_count;
static int output_result;
static int idle;
static int fail_user;
static int missing_scratch;

static void *lookup(void *map, const void *key) {
    if (map == &SCRATCH)
        return missing_scratch ? 0 : &scratch;
    assert(map == &COUNTERS && *(__u32 *)key < 3);
    return &counters[*(__u32 *)key];
}

static __u64 pid_tgid(void) {
    return idle ? 0 : (0x12345678ULL << 32) | 0x23456789;
}

static __u64 timestamp(void) { return 0x0102030405060708ULL; }
static __u32 cpu(void) { return 37; }

static long stack(void *ctx, void *buffer, __u32 size, __u64 flags) {
    assert(ctx == (void *)0x1234 && size == MAX_STACK_DEPTH * sizeof(__u64));
    __u64 *frames = buffer;
    // Deliberately write before failing to exercise partial helper writes.
    frames[0] = 0x1122334455667788ULL;
    if (flags == BPF_F_USER_STACK && !fail_user) {
        frames[1] = 0x8877665544332211ULL;
        return 16;
    }
    return -14;
}

static long output(void *ctx, void *map, __u64 flags, void *data, __u64 size) {
    assert(ctx == (void *)0x1234 && map == &EVENTS);
    assert(flags == BPF_F_CURRENT_CPU && size == sizeof(emitted));
    memcpy(&emitted, data, sizeof(emitted));
    output_count++;
    return output_result;
}

static void no_unused_frames(void) {
    for (int i = emitted.user_depth; i < MAX_STACK_DEPTH; i++)
        assert(emitted.frames[i] == 0);
    for (int i = emitted.kernel_depth; i < MAX_STACK_DEPTH; i++)
        assert(emitted.frames[MAX_STACK_DEPTH + i] == 0);
}

// Scenario: Original kernel C executes with deterministic native helper mocks.
// Guarantees: Wire bytes, padding clearing, flags, idle handling and loss counters
// are checked independently of the Rust encoder and without kernel privileges.
int main(void) {
    bpf_map_lookup_elem = lookup;
    bpf_get_current_pid_tgid = pid_tgid;
    bpf_ktime_get_boot_ns = timestamp;
    bpf_get_smp_processor_id = cpu;
    bpf_get_stack = stack;
    bpf_perf_event_output = output;

    memset(&scratch, 0xa5, sizeof(scratch));
    assert(profile_cpu_kernel((void *)0x1234) == 0);
    assert(counters[0] == 1 && counters[1] == 0 && counters[2] == 0);
    assert(output_count == 1 && emitted.user_depth == 2 && emitted.kernel_depth == 0);
    assert(emitted.flags == FLAG_KERNEL_STACK_ERROR);
    assert(emitted.kernel_stack_error == -14 && emitted.user_stack_error == 0);
    assert(emitted.reserved0 == 0 && emitted.reserved1 == 0);
    no_unused_frames();
    if (fwrite(&emitted, sizeof(emitted), 1, stdout) != 1)
        return 1;

    fail_user = 1;
    output_result = -28;
    assert(profile_cpu((void *)0x1234) == 0);
    assert(counters[0] == 2 && counters[1] == 1);
    assert(emitted.flags == FLAG_USER_STACK_ERROR && emitted.user_stack_error == -14);
    assert(emitted.kernel_stack_error == 0 && emitted.user_depth == 0);
    no_unused_frames();

    idle = 1;
    assert(profile_cpu((void *)0x1234) == 0);
    assert(counters[0] == 3 && counters[1] == 1 && counters[2] == 1);
    assert(output_count == 2);

    idle = 0;
    missing_scratch = 1;
    assert(profile_cpu((void *)0x1234) == 0);
    assert(counters[0] == 4 && counters[1] == 1 && counters[2] == 1);
    assert(output_count == 2);
    return 0;
}
