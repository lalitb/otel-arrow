// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

#include "vmlinux.h"
#include <bpf/bpf_helpers.h>

#define MAX_REUSEPORT_SOCKS 256
#define MAX_NUMA_NODES 64
/* Slot reserved for the global round-robin fallback counter. */
#define GLOBAL_COUNTER_KEY MAX_NUMA_NODES

struct numa_range {
    __u32 start;
    __u32 len;
};

struct {
    __uint(type, BPF_MAP_TYPE_REUSEPORT_SOCKARRAY);
    __uint(max_entries, MAX_REUSEPORT_SOCKS);
    __type(key, __u32);
    __type(value, __u32);
} reuseport_socks SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, MAX_NUMA_NODES);
    __type(key, __u32);
    __type(value, struct numa_range);
} numa_ranges SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, __u32);
} total_sockets SEC(".maps");

/*
 * Per-NUMA round-robin counters at keys 0..MAX_NUMA_NODES-1, plus a single
 * global-fallback counter at key MAX_NUMA_NODES. A shared `BPF_MAP_TYPE_ARRAY`
 * with an atomic fetch-add gives true global per-NUMA round-robin: every new
 * connection landing on the same NUMA node advances the same counter and
 * picks the next listener in that node's range, regardless of which RX CPU
 * the packet arrived on. Values start at 0 because BPF array entries are
 * zero-initialised by the kernel; the loader does not need to seed this map.
 */
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, MAX_NUMA_NODES + 1);
    __type(key, __u32);
    __type(value, __u32);
} selection_counters SEC(".maps");

/*
 * Atomically increment counter[counter_key] and return the previous value
 * modulo `len`. Returns 0 if the map lookup fails or `len` is zero, which is
 * a safe in-range index for any non-empty range; the caller is responsible
 * for not invoking the helper with len == 0 against a real range.
 *
 * `__sync_fetch_and_add` lowers to BPF_ATOMIC_FETCH_ADD and therefore
 * requires BPF ISA v3 (clang `-mcpu=v3`) and Linux >= 5.12. The build script
 * passes `-mcpu=v3` for this object.
 */
static __always_inline __u32 next_index(__u32 counter_key, __u32 len)
{
    __u32 *counter = bpf_map_lookup_elem(&selection_counters, &counter_key);
    if (!counter || len == 0)
        return 0;
    __u32 idx = __sync_fetch_and_add(counter, 1);
    return idx % len;
}

SEC("sk_reuseport")
int select_numa_reuseport(struct sk_reuseport_md *ctx)
{
    __u32 total_key = 0;
    __u32 *total = bpf_map_lookup_elem(&total_sockets, &total_key);
    if (!total || *total == 0)
        return SK_PASS;

    __u32 selected = 0;
    bool used_numa = false;

    __s32 numa = bpf_get_numa_node_id();
    if (numa >= 0 && numa < MAX_NUMA_NODES) {
        __u32 numa_key = (__u32)numa;
        struct numa_range *range = bpf_map_lookup_elem(&numa_ranges, &numa_key);
        if (range && range->len > 0) {
            selected = range->start + next_index(numa_key, range->len);
            used_numa = true;
        }
    }

    /* Global round-robin fallback: NUMA unknown or that node has no listeners. */
    if (!used_numa)
        selected = next_index(GLOBAL_COUNTER_KEY, *total);

    /*
     * Any non-zero return from bpf_sk_select_reuseport leaves the kernel to
     * pick a socket via its default hash; SK_PASS is therefore safe in both
     * paths and matches the documented "selector never drops" invariant.
     */
    (void)bpf_sk_select_reuseport(ctx, &reuseport_socks, &selected, 0);
    return SK_PASS;
}

char LICENSE[] SEC("license") = "GPL";
